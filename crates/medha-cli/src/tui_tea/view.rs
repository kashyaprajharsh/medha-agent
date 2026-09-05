//! Pure rendering for the transcript, composer, status, and overlays.
#![allow(clippy::too_many_arguments)]
use super::*;
use unicode_width::UnicodeWidthStr;

/// The spinner glyph plus the colour it glows at this frame — the two always
/// travel together, so no call site can draw it in a flat colour by accident.
pub(super) fn spinner_span(frame: u64) -> Span<'static> {
    let (glyph, lit) = super::spin::primary_at(frame);
    Span::styled(glyph, Style::default().fg(theme::current().glow(lit)))
}

/// Live-activity verb for a tool category.
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

/// Seconds as "8s" or "1m03s", matching the turn clock.
fn secs_str(s: u64) -> String {
    if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// Token counts at a glance: exact while small, thousands once it stops mattering.
fn tokens_str(tokens: u64) -> String {
    match tokens {
        0 => "—".to_string(),
        t if t < 1_000 => t.to_string(),
        t => format!("{:.1}k", t as f64 / 1_000.0),
    }
}

/// One line naming what an agent is doing now. This is the whole point of the
/// tree: "running" for eleven minutes and "fs.read app.py" for eleven minutes
/// look identical to the operator, and only one of them is a mystery.
fn phase_line(phase: &kernel::Phase) -> (String, Color) {
    match phase {
        kernel::Phase::Generating => ("thinking…".to_string(), theme::dim()),
        kernel::Phase::InTool { tool, target } => (
            match target {
                Some(target) => format!("{tool}  {}", short_target(target)),
                None => tool.clone(),
            },
            theme::dim(),
        ),
        // The one row that is about the operator, so it is the one that is loud.
        kernel::Phase::AwaitingApproval { action } => {
            (format!("⏸ waiting on you — {action}"), theme::warn())
        }
        kernel::Phase::Idle => ("idle".to_string(), theme::faint()),
        kernel::Phase::Settled => ("finished".to_string(), theme::faint()),
    }
}

/// How many rows the switcher needs: a hint line, then one per destination.
pub(super) fn switcher_height(model: &Model) -> u16 {
    match model.switching {
        false => 0,
        true => 1 + model.switch_rows().len() as u16,
    }
}

/// The switcher: every place the transcript can show, the conversation included.
///
/// Two markers, deliberately independent. `❯` is where the keyboard is; `●` is
/// what the transcript area is showing. Browsing the list must not drag the view
/// along with it, or you cannot look for an agent while reading another.
pub(super) fn draw_switcher(f: &mut Frame, model: &Model, area: Rect) {
    if !model.switching || area.height == 0 {
        return;
    }
    let rows = model.switch_rows();
    let hint = match model.focus.is_some() {
        true => "↑↓ select · enter view · x stop · ctrl+k stop all · esc close",
        false => "↑↓ select · enter view · x stop · esc close",
    };
    let mut lines = vec![Line::from(Span::styled(
        format!("  {hint}"),
        Style::default().fg(theme::faint()),
    ))];
    for (index, row) in rows.iter().enumerate() {
        let cursor = if index == model.switch_cursor {
            "❯"
        } else {
            " "
        };
        let shown = if row == &model.focus { "●" } else { "○" };
        let (name, detail) = match row {
            None => ("main".to_string(), String::new()),
            Some(path) => {
                let progress = model.agent_progress.get(path);
                let detail = match progress {
                    Some(progress) => format!(
                        "{} · {} · {}",
                        progress.tool_calls,
                        tokens_str(progress.tokens),
                        progress.phase.label()
                    ),
                    None => String::new(),
                };
                (path.name().to_string(), detail)
            }
        };
        // A child blocked on the operator is the one row that has to catch the
        // eye: nothing else will move it, and it is invisible from the
        // conversation.
        let waiting = row.as_ref().is_some_and(|path| {
            matches!(
                model.agent_progress.get(path).map(|p| &p.phase),
                Some(kernel::Phase::AwaitingApproval { .. })
            )
        });
        let name_style = match (row == &model.focus, waiting) {
            (_, true) => Style::default()
                .fg(theme::warn())
                .add_modifier(Modifier::BOLD),
            (true, _) => Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::BOLD),
            (false, _) => Style::default().fg(theme::text()),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{cursor} "), Style::default().fg(theme::accent())),
            Span::styled(format!("{shown} "), Style::default().fg(theme::accent())),
            Span::styled(name, name_style),
            Span::styled(format!("   {detail}"), Style::default().fg(theme::faint())),
        ]));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// Which pane is on screen, so a stream of tool calls is never anonymous.
pub(super) fn draw_breadcrumb(f: &mut Frame, model: &Model, area: Rect) {
    let Some(path) = &model.focus else {
        return;
    };
    if area.height == 0 {
        return;
    }
    let label = format!(" {path} · esc main · tab switch ");
    let width = label.chars().count() as u16;
    let x = area.x + area.width.saturating_sub(width).min(area.width);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            label,
            Style::default()
                .fg(theme::bg())
                .bg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ))),
        Rect {
            x,
            width: width.min(area.width),
            height: 1,
            ..area
        },
    );
}

/// How many rows [`draw_agent_tree`] needs: a header, then two per agent.
pub(super) fn agent_tree_height(model: &Model) -> u16 {
    match model.agent_runs.len() {
        0 => 0,
        n => 1 + (n as u16 * 2),
    }
}

/// The live fleet, pinned above the composer while children run.
///
/// Pinned rather than appended: a tree that pushed a line per refresh would bury
/// the conversation it is reporting on. The permanent record is one collapsed
/// item written to the transcript when the fleet settles.
pub(super) fn draw_agent_tree(f: &mut Frame, model: &Model, area: Rect) {
    if model.agent_runs.is_empty() || area.height == 0 {
        return;
    }
    let waiting = model
        .agent_progress
        .values()
        .filter(|p| matches!(p.phase, kernel::Phase::AwaitingApproval { .. }))
        .count();
    let g = super::spin::secondary(model.anim_frame);
    let mut header = vec![Span::styled(
        format!("{g} {} agent(s) running", model.agent_runs.len()),
        Style::default()
            .fg(theme::accent())
            .add_modifier(Modifier::BOLD),
    )];
    if waiting > 0 {
        header.push(Span::styled(
            format!("   ⏸ {waiting} waiting on you"),
            Style::default()
                .fg(theme::warn())
                .add_modifier(Modifier::BOLD),
        ));
    }
    // Said on the tree itself, beside the agents it acts on. A way in that has to
    // be known in advance is a way in nobody takes — and until it has been taken
    // once it is worth more than a faint aside, because someone watching three
    // agents work and unable to open one will not guess that a key exists.
    let (hint, hint_style) = match (model.switching, model.switched_before) {
        (true, _) => (
            "   ↑↓ select · enter open · x stop".to_string(),
            Style::default().fg(theme::faint()),
        ),
        (false, true) => (
            "   tab to open".to_string(),
            Style::default().fg(theme::faint()),
        ),
        (false, false) => (
            "   press tab to open one ".to_string(),
            Style::default()
                .fg(theme::bg())
                .bg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
    };
    header.push(Span::styled(hint, hint_style));
    let mut lines = vec![Line::from(header)];

    let last = model.agent_runs.len().saturating_sub(1);
    for (index, run) in model.agent_runs.iter().enumerate() {
        let progress = model.agent_progress.get(&run.path);
        let (branch, cont) = if index == last {
            ("└ ", "  ")
        } else {
            ("├ ", "│ ")
        };
        let elapsed = secs_str(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|now| now.as_secs().saturating_sub(run.started_ms / 1000))
                .unwrap_or(0),
        );
        let counters = match progress {
            Some(p) => format!(
                " · {} tools · {} · {elapsed}",
                p.tool_calls,
                tokens_str(p.tokens)
            ),
            // Absent means the tick has not sampled it yet, which is not the
            // same as an agent that has done nothing — so it says neither.
            None => format!(" · {elapsed}"),
        };
        lines.push(Line::from(vec![
            Span::styled(branch, Style::default().fg(theme::faint())),
            Span::styled(
                run.path.name().to_string(),
                Style::default()
                    .fg(theme::text())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(counters, Style::default().fg(theme::faint())),
        ]));
        let (what, colour) = progress
            .map(|p| phase_line(&p.phase))
            .unwrap_or_else(|| ("starting…".to_string(), theme::faint()));
        lines.push(Line::from(vec![
            Span::styled(format!("{cont}└ "), Style::default().fg(theme::faint())),
            Span::styled(what, Style::default().fg(colour)),
        ]));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// Glyph and colour for a settled agent, matching the roster's vocabulary.
fn status_mark(status: orchestrator::AgentStatus) -> (&'static str, Color) {
    match status {
        orchestrator::AgentStatus::Completed => ("✓", theme::ok()),
        orchestrator::AgentStatus::Exhausted => ("◐", theme::warn()),
        orchestrator::AgentStatus::Cancelled => ("⊘", theme::dim()),
        orchestrator::AgentStatus::Failed => ("✗", theme::err()),
    }
}

/// The record of a finished fan-out: one line, or every child when expanded.
fn render_agents_done(rows: &[AgentDoneRow], expanded: bool) -> Vec<Line<'static>> {
    if rows.is_empty() {
        return Vec::new();
    }
    let tools: u32 = rows.iter().map(|row| row.tool_calls).sum();
    let tokens: u64 = rows.iter().map(|row| row.tokens).sum();
    let longest = rows.iter().map(|row| row.seconds).max().unwrap_or(0);
    // The worst outcome leads: three completions and one failure is a failure
    // you need to see, not a summary that averages it away.
    let worst = rows
        .iter()
        .map(|row| row.status)
        .max_by_key(|status| match status {
            orchestrator::AgentStatus::Failed => 3,
            orchestrator::AgentStatus::Exhausted => 2,
            orchestrator::AgentStatus::Cancelled => 1,
            orchestrator::AgentStatus::Completed => 0,
        })
        .unwrap_or(orchestrator::AgentStatus::Completed);
    let (mark, colour) = status_mark(worst);
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("  {mark} "), Style::default().fg(colour)),
        Span::styled(
            format!("{} agent(s) finished", rows.len()),
            Style::default().fg(theme::text()),
        ),
        Span::styled(
            format!(
                " · {tools} tools · {} · {}",
                tokens_str(tokens),
                secs_str(longest)
            ),
            Style::default().fg(theme::faint()),
        ),
        Span::styled(
            if expanded { "" } else { "   ^E to expand" },
            Style::default().fg(theme::faint()),
        ),
    ])];
    if expanded {
        let last = rows.len().saturating_sub(1);
        for (index, row) in rows.iter().enumerate() {
            let (mark, colour) = status_mark(row.status);
            let branch = if index == last {
                "    └ "
            } else {
                "    ├ "
            };
            lines.push(Line::from(vec![
                Span::styled(branch, Style::default().fg(theme::faint())),
                Span::styled(format!("{mark} "), Style::default().fg(colour)),
                Span::styled(row.name.clone(), Style::default().fg(theme::text())),
                Span::styled(
                    format!(
                        " · {} tools · {} · {}",
                        row.tool_calls,
                        tokens_str(row.tokens),
                        secs_str(row.seconds)
                    ),
                    Style::default().fg(theme::faint()),
                ),
            ]));
        }
    }
    lines
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
    // Claim "thinking" only when reasoning is actually enabled or streaming.
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

/// Draws the active theme's ornament from the continuous animation clock.
pub(super) fn motif_line(frame: u64) -> Line<'static> {
    let p = theme::current();
    let t = spin::track(p.motif);
    let n = t.glyphs.len();
    let head = t.head(frame);

    // A six-cell ramp reads as one moving light instead of separate bands.
    const TAIL: usize = 6;

    let white = Style::default()
        .fg(p.glow(100))
        .add_modifier(Modifier::BOLD);
    let gold = Style::default().fg(p.accent);
    let dim = Style::default().fg(p.glow(0));

    let mut spans = Vec::with_capacity(n);
    for i in 0..n {
        let comet = head.and_then(|h| {
            let d = i.abs_diff(h);
            (d <= TAIL).then(|| {
                let s = Style::default().fg(p.glow(((TAIL - d) * 100 / TAIL) as u16));
                if d == 0 {
                    s.add_modifier(Modifier::BOLD)
                } else {
                    s
                }
            })
        });
        let base = t.glyphs[i];
        let style = match comet {
            Some(s) => s,
            None if t.glow.contains(&base) => white,
            None if t.rim.contains(&base) => gold,
            None => dim,
        };
        spans.push(Span::styled(t.glyph_at(i, frame), style));
    }
    Line::from(spans)
}

/// Solid blocks avoid mixed font weights and baselines in terminal renderers.
pub(super) const LOGO: &str = r#"██   ██ ██████ █████  ██   ██  █████
███ ███ ██     ██  ██ ██   ██ ██   ██
██ █ ██ █████  ██  ██ ███████ ███████
██   ██ ██     ██  ██ ██   ██ ██   ██
██   ██ ██████ █████  ██   ██ ██   ██"#;

/// Darken an rgb toward its shadow (num/den of full brightness). Used to bevel
/// the logo's box-drawing outline beneath the bright block fill.
pub(super) fn shade(rgb: (u8, u8, u8), num: u16, den: u16) -> Color {
    let m = |c: u8| ((c as u16 * num) / den.max(1)) as u8;
    Color::Rgb(m(rgb.0), m(rgb.1), m(rgb.2))
}

/// Builds a logo row with darker outline glyphs for a terminal-safe bevel.
pub(super) fn logo_row(line: &str, rgb: (u8, u8, u8)) -> Vec<Span<'static>> {
    let fill = Style::default()
        .fg(Color::Rgb(rgb.0, rgb.1, rgb.2))
        .add_modifier(Modifier::BOLD);
    let edge = Style::default()
        .fg(shade(rgb, 52, 100))
        .add_modifier(Modifier::BOLD);
    // Coalesce consecutive fill and outline glyphs into spans.
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
    let p = theme::current();
    let t = (model.anim_frame % 60) as i32;
    let level = if t < 30 { t } else { 60 - t };
    // Theme endpoints keep the pulse visible on both light and dark canvases.
    let word = lerp_color(p.word_lo, p.word_hi, level, 30);
    body.push(center_line(
        vec![Span::styled(
            // Romanised, not Devanagari: terminals have no Indic shaping engine,
            // so the `ा` matra cannot attach to `ध` and falls back to a dotted
            // placeholder circle — the wordmark rendered as `मेध◌ा`.
            "◆  medhā · intellect  ◆",
            Style::default().fg(word).add_modifier(Modifier::BOLD),
        )],
        w,
    ));
    // Drop the tallest element first when vertical space is constrained.
    let logo_rows = LOGO.lines().count();
    // wordmark + blank + tagline + blank + veena + blank + hint, plus the art.
    let room_for_logo = (area.height as usize) >= logo_rows + 9;
    if room_for_logo {
        body.push(Line::from(""));
        for (i, line) in LOGO.lines().enumerate() {
            body.push(center_line(
                logo_row(line, p.logo[i.min(p.logo.len() - 1)]),
                w,
            ));
        }
    }
    body.push(Line::from(""));
    body.push(center_line(
        vec![Span::styled(
            "verification-first · open source · your machine, your keys",
            Style::default()
                .fg(theme::dim())
                .add_modifier(Modifier::ITALIC),
        )],
        w,
    ));
    if area.height >= 12 {
        body.push(Line::from(""));
        body.push(center_line(motif_line(model.anim_frame).spans, w));
    }
    body.push(Line::from(""));
    body.push(center_line(
        vec![Span::styled(
            "describe a task below · / for commands · ctrl-d to quit",
            Style::default().fg(theme::faint()),
        )],
        w,
    ));
    let top = (area.height as usize).saturating_sub(body.len()) / 2;
    let mut lines: Vec<Line> = (0..top).map(|_| Line::from("")).collect();
    lines.extend(body);
    f.render_widget(Paragraph::new(lines), area);
}

/// Presentation derived from the tool's declared glyph and category.
#[derive(Clone)]
pub(super) struct ToolViz {
    pub(super) icon: String,
    pub(super) category: ToolCategory,
}

/// Colour for a tool's *category* (glyph is the tool's own, from `ToolViz`).
pub(super) fn cat_color(cat: ToolCategory) -> Color {
    let p = theme::current();
    match cat {
        ToolCategory::Read => p.cat_read,
        ToolCategory::Write => p.warn,
        ToolCategory::Search => p.cat_search,
        ToolCategory::Web => p.cat_web,
        ToolCategory::Shell => p.err,
        ToolCategory::Vcs => p.cat_vcs,
        ToolCategory::Diagnostic => p.warn,
        ToolCategory::Plan => p.accent,
        ToolCategory::Other => p.dim,
    }
}

/// Humanizes the final dotted tool-name segment without a lookup table.
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
            Style::default().fg(theme::dim()),
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
        Span::styled("☰ ", Style::default().fg(theme::accent())),
        Span::styled(
            "Plan",
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {bar} {done}/{total}"),
            Style::default().fg(theme::dim()),
        ),
    ])];
    // Optional one-line note about this update, rendered inline.
    if let Some(exp) = payload.get("explanation").and_then(|v| v.as_str()) {
        if !exp.trim().is_empty() {
            lines.push(Line::from(Span::styled(
                format!("  {}", exp.trim()),
                Style::default()
                    .fg(theme::faint())
                    .add_modifier(Modifier::ITALIC),
            )));
        }
    }
    for s in steps {
        let title = s.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let (mark, style) = match s.get("status").and_then(|v| v.as_str()) {
            Some("completed" | "done") => ("✔", Style::default().fg(theme::ok())),
            // Active step: accent bar + bold, and an arrow so "what's happening now"
            // is unmistakable even when the list scrolls by.
            Some("in_progress") => (
                "▶",
                Style::default()
                    .fg(theme::accent())
                    .add_modifier(Modifier::BOLD),
            ),
            _ => ("○", Style::default().fg(theme::text())),
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
            // Full-height gold bar down the left of every line marks the user turn.
            let mut lines = vec![Line::from("")];
            for l in s.lines() {
                lines.push(Line::from(vec![
                    Span::styled("▌ ", Style::default().fg(theme::accent())),
                    Span::styled(
                        l.to_string(),
                        Style::default()
                            .fg(theme::text())
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
            }
            lines.push(Line::from(""));
            lines
        }
        Item::Assistant(s) => render_assistant(s, cx.width),
        Item::AgentsDone(rows) => render_agents_done(rows, cx.show_summary),
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
                        .fg(theme::text())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(arg, Style::default().fg(theme::dim())),
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
                ("╰ ✗", theme::err(), msg)
            } else {
                ("╰", theme::dim(), crate::result_summary(tool, payload))
            };
            let mut lines = vec![Line::from(vec![
                Span::styled(format!("  {mark} "), Style::default().fg(theme::faint())),
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
                Style::default().fg(theme::warn()),
            ))];
            if cx.show_summary {
                if let Some(s) = summary {
                    for l in s.lines() {
                        lines.push(Line::from(Span::styled(
                            format!("    {l}"),
                            Style::default().fg(theme::dim()),
                        )));
                    }
                }
            }
            lines
        }
        Item::Verify { ok, summary } => {
            let (mark, color) = if *ok {
                ("✔", theme::ok())
            } else {
                ("✗", theme::err())
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
            .map(|l| {
                Line::from(Span::styled(
                    l.to_string(),
                    Style::default().fg(theme::dim()),
                ))
            })
            .collect(),
        Item::Thinking(s) => {
            let style = Style::default()
                .fg(theme::dim())
                .add_modifier(Modifier::ITALIC);
            if !cx.show_thinking {
                return vec![Line::from(Span::styled(
                    "  · reasoning (hidden — /reasoning show)",
                    Style::default()
                        .fg(theme::faint())
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
    let name = card
        .get("name")
        .and_then(|value| value.as_str())
        .unwrap_or("memory");
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
            Style::default()
                .fg(theme::warn())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("│ previous  {previous}")),
        Line::from(format!("│ proposed  {proposed}")),
        Line::from(Span::styled(
            "╰─ keep previous · replace with proposed · merge as a new claim",
            Style::default().fg(theme::dim()),
        )),
    ]
}

/// Renders an approval inline in the scrollable transcript.
pub(super) fn render_approval(
    action: &str,
    detail: Option<&str>,
    sel: usize,
    opts: &[&str],
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("Allow ", Style::default().fg(theme::text())),
            Span::styled(
                tool_label(action).to_string(),
                Style::default()
                    .fg(theme::warn())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("?", Style::default().fg(theme::text())),
        ]),
    ];
    if let Some(detail) = detail {
        lines.push(Line::from(""));
        for l in detail.lines().take(18) {
            let style = if l.starts_with('+') && !l.starts_with("+++") {
                Style::default().fg(theme::add_fg())
            } else if l.starts_with('-') && !l.starts_with("---") {
                Style::default().fg(theme::del_fg())
            } else {
                Style::default().fg(theme::dim())
            };
            lines.push(Line::from(Span::styled(l.to_string(), style)));
        }
        let extra = detail.lines().count().saturating_sub(18);
        if extra > 0 {
            lines.push(Line::from(Span::styled(
                format!("… {extra} more lines"),
                Style::default().fg(theme::faint()),
            )));
        }
    }
    lines.push(Line::from(""));
    for (i, label) in opts.iter().enumerate() {
        if i == sel {
            lines.push(Line::from(vec![
                Span::styled("▌ ", Style::default().fg(theme::accent())),
                Span::styled(
                    format!("{}. ", i + 1),
                    Style::default()
                        .fg(theme::accent())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    label.to_string(),
                    Style::default()
                        .fg(theme::text())
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled(format!("  {}. ", i + 1), Style::default().fg(theme::dim())),
                Span::styled(label.to_string(), Style::default().fg(theme::dim())),
            ]));
        }
    }
    // Explicit ready signal — this line only exists once the options above are built,
    // so seeing it means "ready for input", not "still generating / stuck".
    lines.push(Line::from(vec![
        Span::styled(
            "› ",
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "waiting for your input",
            Style::default().fg(theme::accent()),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        "↑↓ + enter · or press 1/2/3 · n to deny",
        Style::default().fg(theme::faint()),
    )));
    lines
}

#[allow(dead_code)]
pub(super) fn parse_inline_markdown_spans(line: &str, base_style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut current_text = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut idx = 0;

    let mut is_bold = false;
    let mut is_italic = false;
    let mut is_code = false;

    let mut push_current = |text: &mut String, bold: bool, italic: bool, code: bool| {
        if !text.is_empty() {
            let mut style = base_style;
            if code {
                // Palette slot, not a fixed gold: the hardcoded value was tuned
                // for warm ink and all but disappeared on the light parchment.
                style = style.fg(theme::code_fg());
            } else {
                if bold {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if italic {
                    style = style.add_modifier(Modifier::ITALIC);
                }
            }
            spans.push(Span::styled(text.clone(), style));
            text.clear();
        }
    };

    let has_matching = |start_idx: usize, delim: &[char]| -> bool {
        if start_idx + delim.len() > chars.len() {
            return false;
        }
        let mut i = start_idx;
        while i + delim.len() <= chars.len() {
            if chars[i..i + delim.len()] == *delim {
                return true;
            }
            if delim != ['`'] && chars[i] == '`' {
                i += 1;
                while i < chars.len() && chars[i] != '`' {
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }
                continue;
            }
            i += 1;
        }
        false
    };

    while idx < chars.len() {
        if chars[idx] == '`' {
            if is_code {
                push_current(&mut current_text, is_bold, is_italic, is_code);
                is_code = false;
                idx += 1;
            } else if has_matching(idx + 1, &['`']) {
                push_current(&mut current_text, is_bold, is_italic, is_code);
                is_code = true;
                idx += 1;
            } else {
                current_text.push('`');
                idx += 1;
            }
        } else if !is_code && chars[idx] == '*' {
            if idx + 1 < chars.len() && chars[idx + 1] == '*' {
                if is_bold {
                    push_current(&mut current_text, is_bold, is_italic, is_code);
                    is_bold = false;
                    idx += 2;
                } else if has_matching(idx + 2, &['*', '*']) {
                    push_current(&mut current_text, is_bold, is_italic, is_code);
                    is_bold = true;
                    idx += 2;
                } else {
                    current_text.push('*');
                    current_text.push('*');
                    idx += 2;
                }
            } else if is_italic {
                push_current(&mut current_text, is_bold, is_italic, is_code);
                is_italic = false;
                idx += 1;
            } else if has_matching(idx + 1, &['*']) {
                push_current(&mut current_text, is_bold, is_italic, is_code);
                is_italic = true;
                idx += 1;
            } else {
                current_text.push('*');
                idx += 1;
            }
        } else if !is_code && chars[idx] == '_' {
            if idx + 1 < chars.len() && chars[idx + 1] == '_' {
                if is_bold {
                    push_current(&mut current_text, is_bold, is_italic, is_code);
                    is_bold = false;
                    idx += 2;
                } else if has_matching(idx + 2, &['_', '_']) {
                    push_current(&mut current_text, is_bold, is_italic, is_code);
                    is_bold = true;
                    idx += 2;
                } else {
                    current_text.push('_');
                    current_text.push('_');
                    idx += 2;
                }
            } else if is_italic {
                push_current(&mut current_text, is_bold, is_italic, is_code);
                is_italic = false;
                idx += 1;
            } else if has_matching(idx + 1, &['_']) {
                push_current(&mut current_text, is_bold, is_italic, is_code);
                is_italic = true;
                idx += 1;
            } else {
                current_text.push('_');
                idx += 1;
            }
        } else {
            current_text.push(chars[idx]);
            idx += 1;
        }
    }

    push_current(&mut current_text, is_bold, is_italic, is_code);

    if spans.is_empty() {
        vec![Span::styled(line.to_string(), base_style)]
    } else {
        spans
    }
}

#[allow(dead_code)]
pub(super) fn parse_inline_markdown(line: &str) -> Line<'static> {
    let trimmed = line.trim_start();
    let num_hashes = trimmed.chars().take_while(|&c| c == '#').count();
    let rest = &trimmed[num_hashes..];
    if num_hashes > 0 && num_hashes <= 6 && rest.starts_with(' ') {
        let header_content = rest.trim_start();
        let header_style = Style::default()
            .fg(theme::accent())
            .add_modifier(Modifier::BOLD);
        Line::from(parse_inline_markdown_spans(header_content, header_style))
    } else {
        Line::from(parse_inline_markdown_spans(
            line,
            Style::default().fg(theme::text()),
        ))
    }
}

/// Render assistant markdown to logical lines at pane `width`. Backed by the
/// `markdown` module (pulldown-cmark + our themed renderer): tables, code fences,
/// nested lists, blockquotes, links — all in the active palette.
pub(super) fn render_assistant(s: &str, width: u16) -> Vec<Line<'static>> {
    super::markdown::render(s, width)
}

#[allow(dead_code)]
pub(super) fn checklist_line(line: &str) -> Option<Line<'static>> {
    let indent_len = line.len() - line.trim_start().len();
    let indent = &line[..indent_len];
    let rest = &line[indent_len..];
    let body = rest
        .strip_prefix("- ")
        .or_else(|| rest.strip_prefix("* "))?;
    let (mark, color, text, done) = match body
        .strip_prefix("[x]")
        .or_else(|| body.strip_prefix("[X]"))
    {
        Some(t) => ("✔", theme::ok(), t.trim_start(), true),
        None => {
            let t = body.strip_prefix("[ ]")?;
            ("○", theme::dim(), t.trim_start(), false)
        }
    };
    let text_style = if done {
        Style::default().fg(theme::dim())
    } else {
        Style::default().fg(theme::text())
    };
    let mut spans = vec![
        Span::raw(indent.to_string()),
        Span::styled(
            format!("{mark} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ];
    spans.extend(parse_inline_markdown_spans(text, text_style));
    Some(Line::from(spans))
}

/// Renders capped tool JSON using the code-fence visual style.
pub(super) fn json_block(v: &serde_json::Value, label: &str) -> Vec<Line<'static>> {
    let text = serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());
    let border = Style::default().fg(theme::border());
    let mut lines = vec![Line::from(vec![
        Span::styled("    ╭─ ", border),
        Span::styled(
            label.to_string(),
            Style::default()
                .fg(theme::dim())
                .add_modifier(Modifier::BOLD),
        ),
    ])];
    let total = text.lines().count();
    let rows = super::markdown::highlight_lines("json", &text);
    for spans in rows.into_iter().take(MAX_TOOL_OUTPUT_LINES) {
        let mut line = vec![Span::styled("    │ ", border)];
        line.extend(spans);
        lines.push(Line::from(line));
    }
    if total > MAX_TOOL_OUTPUT_LINES {
        let hidden = total - MAX_TOOL_OUTPUT_LINES;
        lines.push(Line::from(Span::styled(
            format!("    ╰ [+{hidden} more lines — toggle /detail]"),
            Style::default().fg(theme::faint()),
        )));
    } else {
        lines.push(Line::from(Span::styled("    ╰─", border)));
    }
    lines
}

pub(super) const MIN_SIDE_BY_SIDE: u16 = 96;

/// Unchanged context lines retained around each change.
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

/// Collapses unchanged diff runs outside the configured hunk context.
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
        Style::default().fg(theme::faint()),
    ))
}

pub(super) fn render_diff(old: &str, new: &str, path: &str, width: u16) -> Vec<Line<'static>> {
    let rows = hunk_rows(old, new);
    let mut lines: Vec<Line<'static>> = Vec::new();
    if !path.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  ✎ ", Style::default().fg(theme::faint())),
            Span::styled(
                path.to_string(),
                Style::default()
                    .fg(theme::dim())
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    let ctx_num = Style::default().fg(theme::lineno());
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
    // Side-by-side layout is useful only when both sides contain changes.
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
                DiffRow::Del(oi, t) => ("-", theme::del_fg(), Some(theme::del_bg()), *oi, t),
                DiffRow::Ins(ni, t) => ("+", theme::add_fg(), Some(theme::add_bg()), *ni, t),
                DiffRow::Ctx(_, ni, t) => (" ", theme::dim(), None, *ni, t),
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
            (theme::del_fg(), Some(theme::del_bg()))
        } else {
            (theme::dim(), None)
        };
        let (rfg, rbg) = if changed && right.is_some() {
            (theme::add_fg(), Some(theme::add_bg()))
        } else {
            (theme::dim(), None)
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
            Span::styled("│ ", Style::default().fg(theme::faint())),
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
            Style::default().fg(theme::faint()),
        )));
    }
    lines
}

pub(super) fn draw_status(f: &mut Frame, model: &Model, area: Rect) {
    let mut left = vec![
        Span::styled("▌ ", Style::default().fg(theme::accent())),
        Span::styled(
            "medha",
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            // The protocol tag is the first thing to go: a long model name plus
            // a long protocol name is what pushes a narrow status line over.
            if area.width >= 100 {
                format!("  {} [{}]", model.model, model.protocol.as_str())
            } else {
                format!("  {}", model.model)
            },
            Style::default().fg(theme::dim()),
        ),
    ];
    // Autonomy badge — always visible so the user knows how much runs without
    // asking. yolo is loud (bold WARN) since it auto-runs edits + shell.
    let (mode_txt, mode_style) = match model.autonomy {
        kernel::AutonomyLevel::Careful => ("careful", Style::default().fg(theme::dim())),
        kernel::AutonomyLevel::Normal => ("normal", Style::default().fg(theme::dim())),
        kernel::AutonomyLevel::Yolo => (
            "⚠ yolo",
            Style::default()
                .fg(theme::warn())
                .add_modifier(Modifier::BOLD),
        ),
    };
    left.push(Span::styled(format!("  [{mode_txt}]"), mode_style));
    // User-owned forms pause the activity indicator.
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
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ));
    } else if model.running {
        left.push(Span::raw("  "));
        left.push(spinner_span(model.anim_frame));
        left.push(Span::styled(
            format!("  {} · {}", activity_label(model), elapsed_str(model)),
            Style::default().fg(theme::warn()),
        ));
    }
    // Live "compacting…" indicator while a summarize pass calls the model.
    if model.compacting {
        left.push(Span::raw("  "));
        left.push(spinner_span(model.anim_frame));
        left.push(Span::styled(
            "  compacting context…",
            Style::default()
                .fg(theme::warn())
                .add_modifier(Modifier::BOLD),
        ));
    }
    // Surface owned background tasks even when no foreground turn is active.
    let running_bg = model.bg_running();
    if running_bg > 0 {
        let g = super::spin::secondary(model.anim_frame);
        let word = if running_bg == 1 { "task" } else { "tasks" };
        left.push(Span::styled(
            format!("  {g} {running_bg} bg {word}"),
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ));
    }
    // The tree above the composer carries the detail; the status line only needs
    // to say a child is blocked on the operator, because nothing else will move
    // it and it is invisible from the conversation.
    let waiting = model
        .agent_progress
        .values()
        .filter(|p| matches!(p.phase, kernel::Phase::AwaitingApproval { .. }))
        .count();
    if waiting > 0 {
        let word = if waiting == 1 { "agent" } else { "agents" };
        left.push(Span::styled(
            format!("  ⏸ {waiting} {word} waiting on you"),
            Style::default()
                .fg(theme::warn())
                .add_modifier(Modifier::BOLD),
        ));
    }
    // Use the cached patch count because status rendering runs every frame.
    let unmerged = model
        .agents
        .as_ref()
        .map(|control| control.cached_unmerged())
        .unwrap_or(0);
    if unmerged > 0 {
        left.push(Span::styled(
            format!("  ⎇ {unmerged} patch(es) — /agents"),
            Style::default()
                .fg(theme::warn())
                .add_modifier(Modifier::BOLD),
        ));
    }
    if let Some((message, until)) = &model.clipboard_status
        && *until > Instant::now()
    {
        left.push(Span::styled(
            format!("  ✓ {message}"),
            Style::default().fg(theme::ok()),
        ));
    }
    let ctx = match model.ctx_pct {
        Some(pct) => format!("ctx {pct}%"),
        None => "ctx —".to_string(),
    };
    // Mark indicative list prices; self-hosted routes may not incur them.
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
    let stream = if model.streaming {
        ""
    } else {
        " · stream off"
    };
    let hints = if model.running {
        "esc interrupt"
    } else {
        "/reasoning · /detail · /help"
    };
    // Pad by terminal cells so wide glyphs preserve right alignment.
    let left_w: usize = left
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    // Drop status details by priority rather than clipping mid-word.
    let available = (area.width as usize).saturating_sub(left_w + 2);
    let right = [
        format!("{ctx}{cost} · {reasoning}{stream}   {hints}"),
        format!("{ctx}{cost} · {reasoning}{stream}"),
        format!("{ctx}{cost} · reasoning {mode}{stream}"),
        format!("{ctx}{cost}"),
        ctx.clone(),
    ]
    .into_iter()
    .find(|candidate| UnicodeWidthStr::width(candidate.as_str()) <= available)
    .unwrap_or_default();

    let pad = available.saturating_sub(UnicodeWidthStr::width(right.as_str())) + 2;
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(right, Style::default().fg(theme::faint())));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Wraps input and positions its cursor using terminal-cell widths.
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

/// Composer placeholder, longest variant that fits `width` cells. Hints are
/// worth showing but not worth truncating: a half-printed shortcut is noise.
fn placeholder(width: usize) -> &'static str {
    const VARIANTS: [&str; 4] = [
        "Ask medha to build, fix, or explain something…   ( / commands · \\+enter newline · ctrl-click opens links )",
        "Ask medha to build, fix, or explain something…   ( / commands · \\+enter newline )",
        "Ask medha to build, fix, or explain something…   ( / for commands )",
        "Ask medha to build, fix, or explain something…",
    ];
    VARIANTS
        .into_iter()
        .find(|variant| UnicodeWidthStr::width(*variant) <= width)
        .unwrap_or("Ask medha…")
}

pub(super) fn draw_input(f: &mut Frame, model: &Model, area: Rect) {
    let (accent, glyph) = if model.running {
        (theme::faint(), "…")
    } else {
        (theme::accent(), "❯")
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        // Horizontal breathing room only. Vertical padding cost two rows of
        // transcript and made an empty composer five rows tall.
        .padding(ratatui::widgets::Padding::new(1, 1, 0, 0));
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
                Span::styled(prompt, Style::default().fg(theme::faint())),
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
                Span::styled(prompt, Style::default().fg(theme::faint())),
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
            // Longest that fits, so the hints never get clipped mid-word. The
            // invitation always survives; the shortcuts are what give way.
            Span::styled(
                placeholder(inner.width.saturating_sub(2) as usize),
                Style::default().fg(theme::faint()),
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
                Span::styled(row, Style::default().fg(theme::text())),
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

/// Repaint a themed panel bg after `Clear` — else an overlay shows the bare
/// terminal bg (a dark box under a light theme).
fn fill_panel(f: &mut Frame, area: Rect) {
    f.render_widget(
        Block::default().style(Style::default().bg(theme::code_bg())),
        area,
    );
}

pub(super) fn draw_autocomplete(f: &mut Frame, model: &Model, input_area: Rect) {
    let matches = command_matches(&model.input);
    if matches.is_empty() {
        return;
    }
    let n = matches.len();
    let sel = model.ac_sel.min(n - 1);
    // Window the menu so it cannot displace the composer.
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
    fill_panel(f, area);
    let mut lines: Vec<Line> = Vec::with_capacity(visible + 1);
    for (i, (c, d)) in matches[start..end].iter().enumerate() {
        let idx = start + i;
        if idx == sel {
            lines.push(Line::from(vec![
                Span::styled("▌ ", Style::default().fg(theme::accent())),
                Span::styled(
                    c.to_string(),
                    Style::default()
                        .fg(theme::accent())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {d}"), Style::default().fg(theme::dim())),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(c.to_string(), Style::default().fg(theme::text())),
                Span::styled(format!("  {d}"), Style::default().fg(theme::faint())),
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
    lines.push(Line::from(Span::styled(
        hint,
        Style::default().fg(theme::faint()),
    )));
    f.render_widget(Paragraph::new(lines), area);
}

pub(super) fn draw_picker(f: &mut Frame, picker: &Picker, input_area: Rect) {
    let labels = picker.kind.labels();
    let n = labels.len();
    // Derive picker height from the rows actually available above the composer.
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
    fill_panel(f, area);

    // Title shows position (e.g. "3/27") when the list is windowed off-screen.
    let mut title = picker.kind.title().trim().to_string();
    if n > visible {
        title = format!("{title}  ({}/{n})", picker.selected + 1);
    }
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        format!("  {title}"),
        Style::default().fg(theme::faint()),
    ))];
    for (offset, label) in labels[start..end].iter().enumerate() {
        let i = start + offset;
        if i == picker.selected {
            lines.push(Line::from(vec![
                Span::styled("▌ ", Style::default().fg(theme::accent())),
                Span::styled(
                    label.clone(),
                    Style::default()
                        .fg(theme::accent())
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        } else {
            lines.push(Line::from(Span::styled(
                format!("  {label}"),
                Style::default().fg(theme::text()),
            )));
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// Renders a wrapped, focus-scrolling structured-question card above the composer.
pub(super) fn draw_clarify(f: &mut Frame, state: &ClarifyState, input_area: Rect) {
    use ratatui::widgets::{Block, BorderType, Borders};

    let q = &state.questions[state.idx];
    let draft = &state.drafts[state.idx];
    let editing = state.entering_other;
    let multi_q = state.questions.len() > 1;

    let mut lines: Vec<Line> = Vec::new();

    // Tabs expose current, answered, and open questions.
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
                        .fg(theme::accent())
                        .add_modifier(Modifier::BOLD),
                )
            } else if answered {
                ("✓", Style::default().fg(theme::text()))
            } else {
                ("○", Style::default().fg(theme::dim()))
            };
            tabs.push(Span::styled(format!("{mark} {label}   "), style));
        }
        lines.push(Line::from(tabs));
        lines.push(Line::from(Span::styled(
            "  ←→ switch questions",
            Style::default().fg(theme::faint()),
        )));
        lines.push(Line::from(""));
    }

    // Inline a lone header; multiple headers already appear as tabs.
    let head = if multi_q || q.header.trim().is_empty() {
        String::new()
    } else {
        format!("[{}] ", q.header.trim())
    };
    lines.push(Line::from(Span::styled(
        format!("{head}{}", q.prompt),
        Style::default()
            .fg(theme::accent())
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from("")); // breathing room

    // Dim options while the free-text field owns focus.
    let options_start = lines.len();
    for (i, opt) in q.options.iter().enumerate() {
        let on = draft.selected.contains(&i);
        let focused = !editing && state.cursor == i;
        let (mark, mark_color) = match (q.multi_select, on) {
            (true, true) => ("☑", theme::ok()),
            (true, false) => ("☐", theme::dim()),
            (false, true) => ("◉", theme::ok()),
            (false, false) => ("○", theme::dim()),
        };
        let bar = if focused { "▌ " } else { "  " };
        let bar_color = if focused {
            theme::accent()
        } else {
            theme::faint()
        };
        let label_style = if editing {
            Style::default().fg(theme::dim())
        } else if focused {
            Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::text())
        };
        let mut spans = vec![
            Span::styled(bar, Style::default().fg(bar_color)),
            Span::styled(
                format!("{mark} "),
                Style::default().fg(if editing { theme::faint() } else { mark_color }),
            ),
            Span::styled(opt.label.clone(), label_style),
        ];
        if opt.recommended {
            spans.push(Span::styled(
                " ★",
                Style::default().fg(if editing {
                    theme::faint()
                } else {
                    theme::accent()
                }),
            ));
        }
        if !opt.description.trim().is_empty() {
            spans.push(Span::styled(
                format!("  — {}", opt.description.trim()),
                Style::default().fg(theme::faint()),
            ));
        }
        lines.push(Line::from(spans));
    }

    // The free-text row becomes an inline editor while active.
    let other_line = lines.len();
    if editing {
        let cursor = state.other_cursor.min(state.other_input.len());
        debug_assert!(state.other_input.is_char_boundary(cursor));
        let (before, after) = state.other_input.split_at(cursor);
        lines.push(Line::from(vec![
            Span::styled("▌ ✎ ", Style::default().fg(theme::accent())),
            Span::styled(before.to_string(), Style::default().fg(theme::text())),
            Span::styled("▏", Style::default().fg(theme::accent())),
            Span::styled(after.to_string(), Style::default().fg(theme::text())),
        ]));
    } else {
        let focused = state.cursor == state.other_row();
        let bar = if focused { "▌ " } else { "  " };
        let bar_color = if focused {
            theme::accent()
        } else {
            theme::faint()
        };
        let mut spans = vec![
            Span::styled(bar, Style::default().fg(bar_color)),
            Span::styled(
                "✎ ",
                Style::default().fg(if focused {
                    theme::accent()
                } else {
                    theme::dim()
                }),
            ),
        ];
        match draft.other.as_deref().filter(|t| !t.is_empty()) {
            Some(t) => {
                spans.push(Span::styled("Other: ", Style::default().fg(theme::dim())));
                spans.push(Span::styled(
                    t.to_string(),
                    Style::default()
                        .fg(theme::text())
                        .add_modifier(Modifier::BOLD),
                ));
            }
            None => spans.push(Span::styled(
                "Other…",
                Style::default().fg(if focused { theme::text() } else { theme::dim() }),
            )),
        }
        lines.push(Line::from(spans));
    }

    if let Some(message) = &state.validation {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  ⚠ {message}"),
            Style::default()
                .fg(theme::warn())
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
        let last = state.idx + 1 == state.questions.len();
        if multi_q && !last {
            format!("  ↑↓ options · {pick} · enter next · ←→ jump · esc skip")
        } else {
            format!("  ↑↓ options · {pick} · enter submit · esc skip")
        }
    };
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        hint,
        Style::default().fg(theme::faint()),
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
        .border_style(Style::default().fg(theme::accent()))
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ))
        .padding(ratatui::widgets::Padding::horizontal(1));

    // Use one wrapped-row count for rendering and layout.
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

    // Scroll within available space so the active row remains reachable.
    let available_height = input_area.y;
    if available_height < 3 {
        return;
    }
    let desired_height = (physical.len() as u16).saturating_add(2);
    let height = desired_height.min(available_height);
    let y = input_area.y - height;
    let area = Rect::new(input_area.x, y, input_area.width, height);
    f.render_widget(ratatui::widgets::Clear, area);
    fill_panel(f, area);
    let inner = block.inner(area);
    let visible_rows = inner.height.max(1) as usize;
    let scroll = focus_row.saturating_sub(visible_rows.saturating_sub(1));
    f.render_widget(block, area);
    f.render_widget(
        Paragraph::new(physical).scroll((scroll.min(u16::MAX as usize) as u16, 0)),
        inner,
    );
}

/// Renders the current model.
pub(super) fn view(f: &mut Frame, model: &mut Model) {
    let area = f.area();
    model.viewport_height = area.height as usize;

    // Paint the canvas first so foreground-only styles inherit the theme.
    f.render_widget(
        Block::default().style(Style::default().bg(theme::bg())),
        area,
    );

    // Clamp a width-scaled gutter for narrow terminals.
    let margin = (area.width / 30).clamp(3, 6);
    let content_w = area.width.saturating_sub(margin * 2);

    // Border and horizontal padding consume four cells of composer width.
    let text_rows = input_rows(model, content_w.saturating_sub(4)) as u16;
    let box_h = text_rows.clamp(1, 8) + 2;
    // The fleet takes the gap above the composer, and gives it back the moment
    // nothing is running, so it costs no screen when there are no children.
    let tree_h = agent_tree_height(model);
    let switch_h = switcher_height(model);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(1 + tree_h),
            Constraint::Length(box_h),
            // The switcher takes the status row's place while it is open, so it
            // never pushes the composer around as agents come and go.
            Constraint::Length(1 + switch_h),
        ])
        .split(area);

    let pad_h = move |area: Rect| {
        let pad = margin.min(area.width / 2);
        Rect {
            x: area.x + pad,
            width: area.width.saturating_sub(pad * 2),
            ..area
        }
    };

    draw_transcript(f, model, pad_h(chunks[0]));
    if tree_h > 0 {
        let gap = chunks[1];
        draw_agent_tree(
            f,
            model,
            pad_h(Rect {
                y: gap.y + 1,
                height: tree_h,
                ..gap
            }),
        );
    }
    draw_input(f, model, pad_h(chunks[2]));
    match switch_h {
        0 => draw_status(f, model, pad_h(chunks[3])),
        _ => draw_switcher(f, model, pad_h(chunks[3])),
    }
    // Last, over the transcript's final row, so it reads as a tab on the divider.
    draw_breadcrumb(
        f,
        model,
        pad_h(Rect {
            y: chunks[0].y + chunks[0].height.saturating_sub(1),
            height: 1,
            ..chunks[0]
        }),
    );

    // Hide inactive menus while an approval owns input.
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

fn highlight_cell_range(line: &Line<'static>, start: usize, end: usize) -> Line<'static> {
    use unicode_width::UnicodeWidthChar;

    let mut spans = Vec::new();
    let mut buffer = String::new();
    let mut current_style = None;
    let mut column = 0usize;
    let mut previous_selected = false;

    for span in &line.spans {
        for character in span.content.chars() {
            let width = character.width().unwrap_or(0);
            let selected = if width == 0 {
                previous_selected
            } else {
                column < end && column.saturating_add(width) > start
            };
            let style = if selected {
                span.style.add_modifier(Modifier::REVERSED)
            } else {
                span.style
            };
            if current_style != Some(style) {
                if let Some(previous) = current_style {
                    spans.push(Span::styled(std::mem::take(&mut buffer), previous));
                }
                current_style = Some(style);
            }
            buffer.push(character);
            previous_selected = selected;
            column = column.saturating_add(width);
        }
    }
    if let Some(style) = current_style {
        spans.push(Span::styled(buffer, style));
    }
    Line::from(spans)
}

pub(super) fn draw_transcript(f: &mut Frame, model: &mut Model, area: Rect) {
    // Scroll math uses transcript height, excluding composer and status rows.
    model.viewport_height = area.height as usize;
    model.transcript_area = area;
    // Any transcript content takes precedence over the splash.
    if model.on_welcome_splash() {
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
    // Recompute memoized physical rows only after content or width changes.
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
        // Approval rows participate in the same physical-row scroll model.
        model.approval_rows = if let Some(pending) = model.pending_approval() {
            let mut rows = render_approval(
                &pending.action,
                pending.detail.as_deref(),
                model.approval_sel,
                pending.responder.options(),
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
                    Style::default().fg(theme::faint()),
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
    } else {
        // A parked pane can shrink while it is off screen (its bounded ring may
        // evict old rows). Never restore an offset beyond its new bottom: that
        // rendered a blank transcript until the user happened to scroll.
        model.scroll_offset = model.scroll_offset.min(model.max_scroll());
    }

    // Build only the visible physical-row window.
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
            spinner_span(model.anim_frame),
            Span::styled(
                format!("  {}…", activity_label(model)),
                Style::default().fg(theme::dim()),
            ),
        ])];
        push_block(&spinner, &mut off, &mut visible);
    }

    if let Some(selection) = model.text_selection {
        for (index, line) in visible.iter_mut().enumerate() {
            let global_row = top + index;
            if let Some((start, end)) = selection_cell_range(selection, global_row) {
                *line = highlight_cell_range(line, start, end);
            }
        }
    }

    // Rows are already wrapped to width — render them directly (no ratatui wrap,
    // no full-buffer scroll).
    let p = Paragraph::new(visible).style(Style::default().fg(theme::text()));
    f.render_widget(p, area);

    // Draw overflow position in the padding gutter, never over transcript text.
    if model.content_height > model.viewport_height && area.right() < f.area().right() {
        use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};
        let gutter = Rect {
            x: area.right(),
            y: area.y,
            width: 1,
            height: area.height,
        };
        let mut state = ScrollbarState::new(model.content_height)
            .viewport_content_length(model.viewport_height)
            .position(model.scroll_offset);
        let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .thumb_symbol("┃")
            .track_style(Style::default().fg(theme::faint()))
            .thumb_style(Style::default().fg(theme::accent()));
        f.render_stateful_widget(bar, gutter, &mut state);
    }

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

    #[test]
    fn test_parse_inline_markdown() {
        use super::parse_inline_markdown;
        use crate::tui_tea::theme;
        use ratatui::style::{Modifier, Style};

        let normal = parse_inline_markdown("hello world");
        assert_eq!(normal.spans.len(), 1);
        assert_eq!(normal.spans[0].content, "hello world");

        let bold = parse_inline_markdown("this is **bold** text");
        assert_eq!(bold.spans.len(), 3);
        assert_eq!(bold.spans[0].content, "this is ");
        assert_eq!(bold.spans[1].content, "bold");
        assert_eq!(
            bold.spans[1].style,
            Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::BOLD)
        );
        assert_eq!(bold.spans[2].content, " text");

        let italic = parse_inline_markdown("this is *italic* text");
        assert_eq!(italic.spans.len(), 3);
        assert_eq!(italic.spans[0].content, "this is ");
        assert_eq!(italic.spans[1].content, "italic");
        assert_eq!(
            italic.spans[1].style,
            Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::ITALIC)
        );
        assert_eq!(italic.spans[2].content, " text");

        let code = parse_inline_markdown("some `code` block");
        assert_eq!(code.spans.len(), 3);
        assert_eq!(code.spans[0].content, "some ");
        assert_eq!(code.spans[1].content, "code");
        assert_eq!(code.spans[1].style.fg, Some(theme::code_fg()));
        assert_eq!(code.spans[2].content, " block");

        let non_matching = parse_inline_markdown("x * y * z");
        assert_eq!(non_matching.spans.len(), 3);
        assert_eq!(non_matching.spans[0].content, "x ");
        assert_eq!(non_matching.spans[1].content, " y ");
        assert_eq!(
            non_matching.spans[1].style,
            Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::ITALIC)
        );
        assert_eq!(non_matching.spans[2].content, " z");

        let header = parse_inline_markdown("#### This is a heading");
        assert_eq!(header.spans.len(), 1);
        assert_eq!(header.spans[0].content, "This is a heading");
        assert_eq!(
            header.spans[0].style,
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD)
        );

        let code_hash_header = parse_inline_markdown("#include <stdio.h>");
        assert_eq!(code_hash_header.spans.len(), 1);
        assert_eq!(code_hash_header.spans[0].content, "#include <stdio.h>");
    }

    #[test]
    fn the_placeholder_never_gets_clipped_mid_hint() {
        use unicode_width::UnicodeWidthStr;
        // Every width yields a complete fitting variant.
        for width in [10usize, 40, 60, 80, 100, 140, 200] {
            let text = super::placeholder(width);
            assert!(
                UnicodeWidthStr::width(text) <= width || width < 12,
                "placeholder {:?} ({} cells) overflows width {width}",
                text,
                UnicodeWidthStr::width(text)
            );
        }
        assert!(super::placeholder(200).contains("ctrl-click"));
        assert!(!super::placeholder(60).contains("ctrl-click"));
        assert!(super::placeholder(60).starts_with("Ask medha"));
    }

    #[test]
    fn the_splash_stays_visible_on_every_canvas() {
        // Avoid mutating the process-wide palette in parallel tests.
        let luma = |(r, g, b): (u8, u8, u8)| 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
        for build in theme::Palette::ALL {
            let p = build();
            for row in p.logo {
                if p.is_dark {
                    assert!(luma(row) > 90.0, "{}: {row:?} is too dark", p.id);
                } else {
                    assert!(luma(row) < 180.0, "{}: {row:?} is too pale", p.id);
                }
            }
        }
        let ramps: Vec<_> = theme::Palette::ALL.iter().map(|b| b().logo).collect();
        assert!(
            ramps.windows(2).any(|w| w[0] != w[1]),
            "the logo must re-colour with the palette"
        );
    }

    #[test]
    fn every_motif_uses_only_glyphs_that_render() {
        // Check each motif independently of the active palette.
        for motif in [
            theme::Motif::Veena,
            theme::Motif::Loom,
            theme::Motif::Chisel,
        ] {
            let t = super::super::spin::track(motif);
            for g in t.glyphs.iter().chain(std::iter::once(&t.head_glyph)) {
                for bad in ['━', '┿', '╮', '╯'] {
                    assert!(!g.contains(bad), "{motif:?} uses {bad:?}");
                }
            }
        }
        let t = super::super::spin::track(theme::current().motif);
        let drawn: String = super::motif_line(0)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(drawn.chars().count(), t.glyphs.len());
        for ch in drawn.chars() {
            let s = ch.to_string();
            assert!(
                t.glyphs.contains(&s.as_str()) || s == t.head_glyph,
                "{s:?} is not part of this motif"
            );
        }
    }
}

#[cfg(test)]
mod agent_view_tests {
    use super::*;

    fn row(name: &str, status: orchestrator::AgentStatus, tools: u32, tokens: u64) -> AgentDoneRow {
        AgentDoneRow {
            name: name.into(),
            status,
            tool_calls: tools,
            tokens,
            seconds: 75,
        }
    }

    fn text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_phase_names_the_tool_and_its_target() {
        let (label, _) = phase_line(&kernel::Phase::InTool {
            tool: "fs.read".into(),
            target: Some("/long/path/to/app.py".into()),
        });
        assert_eq!(label, "fs.read  app.py", "the basename, not the whole path");

        let (label, _) = phase_line(&kernel::Phase::Generating);
        assert_eq!(label, "thinking…");
    }

    #[test]
    fn waiting_on_a_person_is_the_loud_row() {
        let (label, colour) = phase_line(&kernel::Phase::AwaitingApproval {
            action: "shell: npm ls".into(),
        });
        assert!(label.contains("waiting on you"), "{label}");
        assert!(
            label.contains("npm ls"),
            "it must say what it is waiting for"
        );
        assert_eq!(
            colour,
            theme::warn(),
            "a blocked agent is the one row that must catch the eye"
        );
    }

    fn running(name: &str) -> orchestrator::Agent {
        orchestrator::Agent {
            path: orchestrator::AgentPath::root().child(name).unwrap(),
            session: ulid::Ulid::new().to_string(),
            objective: "work".into(),
            started_ms: 0,
            state: orchestrator::State::Running,
            write: false,
            tools: None,
        }
    }

    #[test]
    fn the_tree_costs_no_rows_when_nothing_is_running() {
        let dir = std::env::temp_dir().join(format!("medha-view-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut model = Model::new(
            "m".into(),
            None,
            kernel::ReasoningConfig::default(),
            lockfile::UiConfig::default(),
            HashMap::new(),
            Arc::new(WorkspaceSandbox::new_jailed(&dir).unwrap()),
        );
        assert_eq!(
            agent_tree_height(&model),
            0,
            "no children, no screen taken from the conversation"
        );
        model.agent_runs = vec![running("worker")];
        assert_eq!(
            agent_tree_height(&model),
            3,
            "header plus two rows per agent"
        );
    }

    #[test]
    fn a_finished_fanout_collapses_to_one_line_and_expands_on_request() {
        let rows = vec![
            row("overview", orchestrator::AgentStatus::Completed, 12, 18_400),
            row("backend", orchestrator::AgentStatus::Completed, 24, 43_100),
        ];
        let collapsed = text(&render_agents_done(&rows, false));
        assert_eq!(collapsed.lines().count(), 1, "{collapsed}");
        assert!(collapsed.contains("2 agent(s) finished"), "{collapsed}");
        assert!(collapsed.contains("36 tools"), "counters sum: {collapsed}");
        assert!(collapsed.contains("61.5k"), "{collapsed}");
        assert!(
            collapsed.contains("^E"),
            "the way to expand must be on the row"
        );

        let expanded = text(&render_agents_done(&rows, true));
        assert_eq!(expanded.lines().count(), 3, "summary plus a row each");
        assert!(expanded.contains("overview") && expanded.contains("backend"));
        assert!(!expanded.contains("^E"), "already expanded");
    }

    #[test]
    fn the_worst_outcome_leads_the_summary() {
        let rows = vec![
            row("a", orchestrator::AgentStatus::Completed, 1, 10),
            row("b", orchestrator::AgentStatus::Failed, 1, 10),
            row("c", orchestrator::AgentStatus::Completed, 1, 10),
        ];
        let collapsed = text(&render_agents_done(&rows, false));
        // Averaging three successes and a failure into a tick would hide the one
        // result the operator has to act on.
        assert!(collapsed.contains('✗'), "{collapsed}");
    }

    #[test]
    fn token_counts_stay_readable_across_magnitudes() {
        assert_eq!(tokens_str(0), "—");
        assert_eq!(tokens_str(940), "940");
        assert_eq!(tokens_str(43_100), "43.1k");
        assert_eq!(secs_str(9), "9s");
        assert_eq!(secs_str(75), "1m15s");
    }
}
