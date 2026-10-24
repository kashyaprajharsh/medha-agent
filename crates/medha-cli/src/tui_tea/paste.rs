//! A paste that arrives as separate key presses becomes one paste, so Enter in it never sends and Esc never closes.

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use std::time::Duration;

/// Faster than anyone types, slower than a terminal hands over a paste.
pub(super) const BURST_GAP: Duration = Duration::from_millis(5);
/// A burst longer than this is handed on as it stands, so a stuck key cannot hold the loop.
pub(super) const MOST_EVENTS: usize = 200_000;

/// The text a key press types, if it types any.
pub(super) fn typed(event: &Event) -> Option<char> {
    let Event::Key(key) = event else { return None };
    if key.kind == KeyEventKind::Release
        || key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    match key.code {
        KeyCode::Char(c) => Some(c),
        KeyCode::Enter => Some('\n'),
        KeyCode::Tab => Some('\t'),
        _ => None,
    }
}

/// Whether a burst may follow this event: a typed key, or the Esc that opens a paste's markers.
pub(super) fn may_start_burst(event: &Event) -> bool {
    typed(event).is_some() || is_esc(event)
}

fn is_esc(event: &Event) -> bool {
    matches!(event, Event::Key(key) if key.code == KeyCode::Esc && key.kind != KeyEventKind::Release)
}

fn is_release(event: &Event) -> bool {
    matches!(event, Event::Key(key) if key.kind == KeyEventKind::Release)
}

/// Events that arrived together, with each paste in them made one event; everything else is unchanged.
pub(super) fn coalesce(events: Vec<Event>) -> Vec<Event> {
    let events: Vec<Event> = events
        .into_iter()
        .filter(|event| !is_release(event))
        .collect();
    let mut out = Vec::new();
    let mut run: Vec<Event> = Vec::new();
    let mut marked = false;
    let mut at = 0;
    while at < events.len() {
        if let Some(opens) = marker(&events[at..]) {
            flush(&mut out, &mut run, marked);
            marked = opens;
            at += 6;
            continue;
        }
        if typed(&events[at]).is_some() {
            run.push(events[at].clone());
        } else {
            flush(&mut out, &mut run, marked);
            out.push(events[at].clone());
        }
        at += 1;
    }
    flush(&mut out, &mut run, marked);
    out
}

/// `Esc [200~` opens a bracketed paste and `Esc [201~` closes it, when a terminal sends them as keys.
fn marker(events: &[Event]) -> Option<bool> {
    let (esc, rest) = events.split_first()?;
    if !is_esc(esc) || rest.len() < 5 {
        return None;
    }
    let text: String = rest[..5].iter().filter_map(typed).collect();
    match text.as_str() {
        "[200~" => Some(true),
        "[201~" => Some(false),
        _ => None,
    }
}

fn flush(out: &mut Vec<Event>, run: &mut Vec<Event>, marked: bool) {
    // Text and Enter written at once (scripts, tmux, an editor sending a line) still sends.
    let enter = (!marked && run.len() > 1 && typed(run.last().expect("non-empty")) == Some('\n'))
        .then(|| run.pop().expect("non-empty"));
    match run.len() {
        0 => {}
        1 if !marked => out.push(run.remove(0)),
        _ => out.push(Event::Paste(
            run.drain(..).filter_map(|event| typed(&event)).collect(),
        )),
    }
    out.extend(enter);
}

#[cfg(test)]
#[path = "paste_tests.rs"]
mod tests;
