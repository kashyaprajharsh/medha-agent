use super::*;
use crate::tokens::HeuristicCounter;
use kernel::{Message, Session};

/// After a compaction the model is reminded of what the person asked. A mode's
/// instruction stored with a message is not theirs, and must not come back as a
/// standing direction once the mode is left.
#[test]
fn notes_quote_what_the_person_typed_not_what_a_mode_added() {
    let session = Session::new();
    let mut planned = Message::user("Do not implement changes.\n\nfix the login bug");
    planned.instruction_prefix = Some("Do not implement changes.\n\n".into());
    let events = [
        Event::user_input_message(&session, &planned),
        Event::user_message(&session, "now implement it"),
    ];

    let notes = history_notes(&events, 4_000, &HeuristicCounter);

    assert!(notes.contains("fix the login bug"), "{notes}");
    assert!(notes.contains("now implement it"), "{notes}");
    assert!(!notes.contains("Do not implement"), "{notes}");
}
