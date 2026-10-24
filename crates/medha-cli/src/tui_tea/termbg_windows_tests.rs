use super::*;
use windows_sys::Win32::System::Console::{
    INPUT_RECORD_0, KEY_EVENT_RECORD, KEY_EVENT_RECORD_0, MOUSE_EVENT, MOUSE_EVENT_RECORD,
};

fn key(unit: u16, down: bool) -> INPUT_RECORD {
    INPUT_RECORD {
        EventType: KEY_EVENT as u16,
        Event: INPUT_RECORD_0 {
            KeyEvent: KEY_EVENT_RECORD {
                bKeyDown: down.into(),
                wRepeatCount: 1,
                uChar: KEY_EVENT_RECORD_0 { UnicodeChar: unit },
                ..Default::default()
            },
        },
    }
}

fn typed(text: &str) -> Vec<INPUT_RECORD> {
    text.encode_utf16()
        .flat_map(|unit| [key(unit, true), key(unit, false)])
        .collect()
}

/// An event as the person would notice it: its kind, press or release, and character.
fn seen(event: &INPUT_RECORD) -> (u16, bool, u16) {
    if event.EventType != KEY_EVENT as u16 {
        return (event.EventType, false, 0);
    }
    let key = unsafe { event.Event.KeyEvent };
    (event.EventType, key.bKeyDown != 0, unsafe {
        key.uChar.UnicodeChar
    })
}

fn taken(events: &[INPUT_RECORD]) -> Taken {
    let mut taken = Taken::new(std::ptr::null_mut());
    for event in events {
        taken.take(*event);
    }
    taken
}

/// Anything typed during the probe, mouse and non-ASCII included, must reach the TUI in order.
#[test]
fn only_the_replys_key_presses_leave_the_console_queue() {
    let mouse = INPUT_RECORD {
        EventType: MOUSE_EVENT as u16,
        Event: INPUT_RECORD_0 {
            MouseEvent: MOUSE_EVENT_RECORD::default(),
        },
    };
    let reply = typed("\x1b]11;rgb:0c0c/0c0c/0c0c\x1b\\");
    let events: Vec<INPUT_RECORD> = [typed("a"), reply.clone(), vec![mouse], typed("éb")].concat();
    let expected: Vec<_> = [
        typed("a"),
        reply
            .iter()
            .filter(|event| !seen(event).1)
            .copied()
            .collect(),
        vec![mouse],
        typed("éb"),
    ]
    .concat()
    .iter()
    .map(seen)
    .collect();

    let mut probe = taken(&events);
    assert_eq!(probe.kept().iter().map(seen).collect::<Vec<_>>(), expected);
    // The test has no console to write into.
    probe.events.clear();
}

/// Without a complete reply nothing is removed, so a terminal that is slow or silent loses no input.
#[test]
fn without_a_whole_reply_every_event_goes_back() {
    let events = [typed("ab"), typed("\x1b]11;rgb:0c0c/0c0c")].concat();
    let mut probe = taken(&events);
    assert_eq!(
        probe.kept().iter().map(seen).collect::<Vec<_>>(),
        events.iter().map(seen).collect::<Vec<_>>()
    );
    probe.events.clear();
}
