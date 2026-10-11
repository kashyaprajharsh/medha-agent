//! What the model reads for a built-in tool's result. The full result stays in
//! the record for people and for saved versions; the model gets the part it
//! acts on, in the form it reads best, and never the same text twice.

use serde_json::Value;
use similar::{ChangeTag, TextDiff};

/// Lines of unchanged text kept around each change in a diff.
const CONTEXT: usize = 3;
/// Longest line shown whole. A minified or generated file's lines are cut,
/// with the cut marked, so one line cannot fill a page.
const MAX_LINE: usize = 2_000;

/// A command's result as text. Errors come before results: when a long result
/// is cut to its start and end, the start shows the first errors and the end
/// shows how the run finished. Fields beyond the streams pass through as is.
pub(crate) fn shell_run(payload: &Value) -> Option<String> {
    let object = payload.as_object()?;
    let command = payload["command"].as_str()?;
    // Where it ran is the model's own choice, or the workspace; the record keeps it.
    let mut view = format!("$ {command}\n");
    match payload["exit_code"].as_i64() {
        Some(code) => view.push_str(&format!("exit code: {code}\n")),
        None => view.push_str("exit code: none (the command did not finish)\n"),
    }
    const SHOWN: [&str; 7] = [
        "command",
        "workdir",
        "exit_code",
        "stdout",
        "stderr",
        "stdout_truncated",
        "stderr_truncated",
    ];
    for (key, value) in object
        .iter()
        .filter(|(key, _)| !SHOWN.contains(&key.as_str()))
    {
        match value {
            Value::Null => {}
            Value::String(text) => view.push_str(&format!("{key}: {text}\n")),
            other => view.push_str(&format!("{key}: {other}\n")),
        }
    }
    for stream in ["stderr", "stdout"] {
        let text = clean_terminal(payload[stream].as_str().unwrap_or_default());
        if text.trim().is_empty() {
            continue;
        }
        view.push_str(&format!("--- {stream} ---\n"));
        if payload[format!("{stream}_truncated")] == true {
            view.push_str("[earlier output was dropped; the last part is kept]\n");
        }
        view.push_str(text.trim_end_matches('\n'));
        view.push('\n');
    }
    Some(view.trim_end().to_owned())
}

/// Terminal output as a reader sees it once it settles: colour and cursor codes
/// removed, and a line redrawn with carriage returns (a progress bar) kept only
/// as its last state, instead of every frame of it.
pub(crate) fn clean_terminal(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            plain.push(c);
            continue;
        }
        match chars.next() {
            // Control sequence: parameters, then one final byte in @..~.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // Operating-system command: up to BEL or ESC \.
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            // Character-set choice: one more character.
            Some('(' | ')') => {
                chars.next();
            }
            _ => {}
        }
    }
    plain
        .split('\n')
        .map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            line.rsplit('\r').next().unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The lines of a page as the model is shown them, without their numbers: each
/// exact, except that one longer than [`MAX_LINE`] is cut, with the cut marked.
pub(crate) fn page_lines(content: &str) -> impl Iterator<Item = std::borrow::Cow<'_, str>> {
    content.split_inclusive('\n').map(|line| {
        let text = line.strip_suffix('\n').unwrap_or(line);
        if text.len() <= MAX_LINE {
            return std::borrow::Cow::Borrowed(text);
        }
        let mut cut = MAX_LINE;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        std::borrow::Cow::Owned(format!(
            "{} … [line cut: {} of {} bytes shown]",
            &text[..cut],
            cut,
            text.len()
        ))
    })
}

/// A page of a file as numbered lines, `N|text`, then where the next page
/// starts. The numbers are not part of the file; lines are otherwise exact.
pub(crate) fn file_page(payload: &Value) -> Option<String> {
    let content = payload["content"].as_str()?;
    let start = payload["start_line"].as_u64()?;
    let mut view = String::with_capacity(content.len() + content.len() / 6);
    let mut number = start;
    for line in page_lines(content) {
        view.push_str(&number.to_string());
        view.push('|');
        view.push_str(&line);
        view.push('\n');
        number += 1;
    }
    if let Some(note) = payload["note"].as_str() {
        view.push_str(&format!("[{note}]\n"));
    }
    let end = number.saturating_sub(1);
    let total = payload["total_lines"].as_u64();
    if payload["has_more"] == true {
        let of = total
            .map(|total| format!(" of {total}"))
            .unwrap_or_default();
        view.push_str(&format!(
            "[lines {start}-{end}{of}; continue with offset={}]",
            end + 1
        ));
    } else if content.is_empty() && payload["note"].is_null() {
        view.push_str("[empty file]");
    }
    Some(view)
}

/// A file change from `write`, `edit` or a multi-edit: what changed, as a
/// unified diff with a little context, plus anything the edit should know.
/// Content written where there was none is named, not echoed: the model wrote
/// every line of it.
pub(crate) fn file_change(payload: &Value) -> Option<String> {
    let path = payload["path"].as_str()?;
    let before = payload["old"].as_str()?;
    let after = payload["new"].as_str()?;
    let diff = TextDiff::from_lines(before, after);
    let (mut added, mut removed) = (0, 0);
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    let mut view = if before.is_empty() && !after.is_empty() {
        format!(
            "Wrote {path} ({} lines, {} bytes).",
            after.lines().count(),
            after.len()
        )
    } else if added + removed == 0 {
        format!("{path} is unchanged.")
    } else {
        let mut summary = format!("Changed {path}: +{added} -{removed} lines");
        if let Some(count) = payload["replacements"].as_u64() {
            summary.push_str(&format!(", {count} replacement(s)"));
        }
        if let Some(count) = payload["edits_applied"].as_u64() {
            summary.push_str(&format!(", {count} edit(s)"));
        }
        let hunks = diff
            .unified_diff()
            .context_radius(CONTEXT)
            .header(path, path)
            .to_string();
        format!("{summary}.\n{hunks}")
    };
    for key in ["note", "preview_notice"] {
        if let Some(text) = payload[key].as_str() {
            view.push_str(&format!("\nNote: {text}"));
        }
    }
    if let Some(report) = payload.get("lsp").filter(|report| !report.is_null()) {
        view.push_str(&format!("\nDiagnostics after the change: {report}"));
    }
    Some(view)
}

#[cfg(test)]
#[path = "model_view_tests.rs"]
mod tests;
