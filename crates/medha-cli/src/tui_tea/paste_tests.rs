use super::*;
use crossterm::event::{KeyEvent, KeyEventState};

fn key(code: KeyCode, kind: KeyEventKind) -> Event {
    Event::Key(KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind,
        state: KeyEventState::NONE,
    })
}

fn press(code: KeyCode) -> Event {
    key(code, KeyEventKind::Press)
}

/// How Windows hands over a paste: each character pressed and released.
fn keys(text: &str) -> Vec<Event> {
    text.chars()
        .flat_map(|c| {
            let code = match c {
                '\n' => KeyCode::Enter,
                '\t' => KeyCode::Tab,
                c => KeyCode::Char(c),
            };
            [
                key(code, KeyEventKind::Press),
                key(code, KeyEventKind::Release),
            ]
        })
        .collect()
}

#[test]
fn a_key_pasted_as_key_presses_arrives_as_one_paste() {
    assert_eq!(
        coalesce(keys("sk-abc123")),
        [Event::Paste("sk-abc123".into())]
    );
}

/// Enter inside a paste is a line in it; sending it would run half a message.
#[test]
fn enter_inside_a_paste_is_a_new_line_not_a_send() {
    assert_eq!(
        coalesce(keys("one\ntwo")),
        [Event::Paste("one\ntwo".into())]
    );
}

/// A line and its Enter written at once, as tmux or a script sends them, still sends the line.
#[test]
fn a_line_written_with_its_enter_still_sends() {
    let line: Vec<Event> = "FROM_TUI"
        .chars()
        .map(|c| press(KeyCode::Char(c)))
        .chain([press(KeyCode::Enter)])
        .collect();
    assert_eq!(
        coalesce(line),
        [Event::Paste("FROM_TUI".into()), press(KeyCode::Enter)]
    );
    assert_eq!(
        coalesce(vec![press(KeyCode::Char('y')), press(KeyCode::Enter)]),
        [press(KeyCode::Char('y')), press(KeyCode::Enter)]
    );
}

/// The Esc that opens a paste's markers would leave the card; it must never reach the screen.
#[test]
fn bracketed_markers_sent_as_keys_wrap_one_paste_and_vanish() {
    let mut events = vec![press(KeyCode::Esc)];
    events.extend(keys("[200~nvapi-1"));
    events.push(press(KeyCode::Esc));
    events.extend(keys("[201~"));
    assert_eq!(coalesce(events), [Event::Paste("nvapi-1".into())]);

    let mut single = vec![press(KeyCode::Esc)];
    single.extend(keys("[200~x"));
    single.push(press(KeyCode::Esc));
    single.extend(keys("[201~"));
    assert_eq!(
        coalesce(single),
        [Event::Paste("x".into())],
        "a one-character paste is still a paste"
    );
}

/// What a person types, one key at a time, reaches the screen exactly as before.
#[test]
fn single_keys_and_other_events_pass_through_unchanged() {
    for event in [
        press(KeyCode::Char('q')),
        press(KeyCode::Enter),
        press(KeyCode::Esc),
        press(KeyCode::Up),
    ] {
        assert_eq!(coalesce(vec![event.clone()]), [event]);
    }
    let mixed = vec![
        press(KeyCode::Up),
        press(KeyCode::Char('a')),
        press(KeyCode::Down),
    ];
    assert_eq!(coalesce(mixed.clone()), mixed);
    let ctrl_v = Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
    assert_eq!(
        coalesce(vec![ctrl_v.clone(), ctrl_v.clone()]),
        [ctrl_v.clone(), ctrl_v]
    );
}

/// A real paste event, as macOS and Linux terminals send it, is left alone.
#[test]
fn a_paste_event_is_kept_as_it_came() {
    let paste = Event::Paste("from the terminal".into());
    assert_eq!(coalesce(vec![paste.clone()]), [paste]);
}
