use super::*;
use crate::{Event, OutputReference, Session};

#[test]
fn plan_note_survives_replay_but_stays_hidden_with_an_output_reference() {
    let mut message = Message::user("About the diagram. Make it blue.");
    message.about = Some(OutputReference {
        artifact_id: "artifact".into(),
        revision_id: "revision".into(),
        label: "Diagram".into(),
        lead: "About the diagram. ".len(),
    });
    apply(&mut message, true);
    let once = message.content.clone();
    apply(&mut message, true);
    assert_eq!(message.content, once);
    assert_eq!(message.shown(), ("Make it blue.", Some("Diagram")));
    let event = Event::user_input_message(&Session::new(), &message);
    assert_eq!(event.shown_text(), message.shown());
    let replayed = crate::project_messages(std::slice::from_ref(&event));
    assert_eq!(replayed[0].shown(), message.shown());
    assert_eq!(replayed[0].content, once);
    assert_eq!(
        crate::project_ordered_messages(&[event]),
        vec![message.ordered()]
    );
    apply(&mut message, false);
    assert_eq!(message.content, once);
}

#[test]
fn reports_cannot_hide_their_text_with_instruction_metadata() {
    let mut report = Message::user("report").carrying(TrustLabel::Web);
    apply(&mut report, true);
    assert!(report.instruction_prefix.is_none());
    report.instruction_prefix = Some("rep".into());
    assert_eq!(report.shown(), ("report", None));
    let event = Event::user_input_message(&Session::new(), &report);
    assert_eq!(event.shown_text(), ("report", None));
    assert!(event.payload.get("instruction_prefix").is_none());
}
