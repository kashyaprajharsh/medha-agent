use super::{escalate_for_trust_flow, follows_web};
use crate::types::{BlastRadius, Containment, Decision};

#[test]
fn escalates_web_tainted_consequential_action_in_a_leaky_box() {
    // Irreversible action, web-tainted, FS jail but network reachable → gate.
    let d = escalate_for_trust_flow(
        Decision::Allow,
        Some(BlastRadius::IrreversibleLocal),
        true,
        Containment::OsFsJail,
    );
    assert!(matches!(d, Decision::Human));
    // External action on the bare host → gate.
    let d = escalate_for_trust_flow(
        Decision::Allow,
        Some(BlastRadius::External),
        true,
        Containment::None,
    );
    assert!(matches!(d, Decision::Human));
}

#[test]
fn no_escalation_when_the_box_confines_network() {
    // A network-denied jail can't exfiltrate, so the same action runs freely.
    let d = escalate_for_trust_flow(
        Decision::Allow,
        Some(BlastRadius::IrreversibleLocal),
        true,
        Containment::OsFsJailNoNet,
    );
    assert!(matches!(d, Decision::Allow));
}

#[test]
fn no_escalation_without_taint_or_for_low_risk_classes() {
    // No web taint → unchanged.
    assert!(matches!(
        escalate_for_trust_flow(
            Decision::Allow,
            Some(BlastRadius::IrreversibleLocal),
            false,
            Containment::None
        ),
        Decision::Allow
    ));
    // Read-class is never consequential.
    assert!(matches!(
        escalate_for_trust_flow(
            Decision::Allow,
            Some(BlastRadius::Read),
            true,
            Containment::None
        ),
        Decision::Allow
    ));
    // Reversible-local (snapshotted + jailed) is left alone to avoid nagging.
    assert!(matches!(
        escalate_for_trust_flow(
            Decision::Allow,
            Some(BlastRadius::ReversibleLocal),
            true,
            Containment::None
        ),
        Decision::Allow
    ));
}

#[test]
fn only_tightens_never_relaxes() {
    // A denial stays denied even under taint (escalation is one-directional).
    let d = escalate_for_trust_flow(
        Decision::Deny {
            reason: "blocked".into(),
        },
        Some(BlastRadius::External),
        true,
        Containment::None,
    );
    assert!(matches!(d, Decision::Deny { .. }));
}

/// A call policy already sends to a human is still a call that follows web
/// text. Its card must be marked, or a remembered "always" would answer it.
#[test]
fn a_call_already_sent_to_a_human_is_still_marked_as_following_the_web() {
    let radius = Some(BlastRadius::External);
    let asked = escalate_for_trust_flow(Decision::Human, radius, true, Containment::None);
    assert!(matches!(asked, Decision::Human));
    assert!(follows_web(radius, true, Containment::None));
    assert!(!follows_web(radius, false, Containment::None));
    assert!(!follows_web(
        Some(BlastRadius::ReversibleLocal),
        true,
        Containment::None
    ));
}
