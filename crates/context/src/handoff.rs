use crate::tokens::TokenCounter;
use kernel::{Event, EventKind, TrustLabel};

pub(crate) const NOTES_MARKER: &str = "\n\n[MEDHA durable user context]\n";
pub(crate) const SUMMARY_FRAME: &str = "[Summary of earlier work, for reference. The latest user message decides what to do now; do not finish items from this summary unless asked.]\n\n";

/// Regenerate quotes from original events so repeated summaries cannot paraphrase them.
pub(crate) fn history_notes(events: &[Event], budget: u32, counter: &dyn TokenCounter) -> String {
    let Some(first) = events.first() else {
        return String::new();
    };
    let users: Vec<_> = events
        .iter()
        .filter_map(|event| {
            if event.kind != EventKind::UserMessage || event.trust != TrustLabel::User {
                return None;
            }
            let text = event.payload.get("text")?.as_str()?;
            (!text.trim().is_empty()).then_some((event.id, text))
        })
        .collect();
    let header = format!(
        "{NOTES_MARKER}Historical user messages in original order; newer corrections supersede older directions. \
         Quotes may be excerpts. Recover omitted details with sessions.search using session_id=\"{}\" \
         and around_event_id, or query.\n",
        first.session_id,
    );
    if counter.count(&header) >= budget {
        return String::new();
    }
    let mut selected = Vec::new();
    let mut remaining = budget - counter.count(&header);
    // Keep the initial task, then prioritize recent corrections. Never let a huge
    // pasted request consume the entire preservation budget.
    for index in (0..users.len().min(1)).chain((1..users.len()).rev()) {
        let (id, text) = users[index];
        let mut excerpt: String = text.chars().take(2_000).collect();
        let per_message = (budget / 4).clamp(1, 768);
        while counter.count(&excerpt) > per_message {
            excerpt = excerpt.chars().take(excerpt.chars().count() / 2).collect();
        }
        let record = serde_json::json!({
            "event_id": id.to_string(), "text": excerpt,
            "truncated": excerpt.len() < text.len(),
        });
        let line = format!("{record}\n");
        let tokens = counter.count(&line);
        if tokens <= remaining {
            remaining -= tokens;
            selected.push((index, line));
        } else if index != 0 {
            break;
        }
        if remaining < 64 {
            break;
        }
    }
    if selected.is_empty() {
        return header;
    }
    selected.sort_unstable_by_key(|(index, _)| *index);
    let mut notes = header;
    for (_, line) in selected {
        notes.push_str(&line);
    }
    notes
}

pub(crate) fn summary_body(summary: &str) -> &str {
    let summary = summary.strip_prefix(SUMMARY_FRAME).unwrap_or(summary);
    summary
        .rsplit_once(NOTES_MARKER)
        .map_or(summary, |(body, _)| body)
}
