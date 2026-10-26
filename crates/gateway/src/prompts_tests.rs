use super::*;
use protocol::{ApprovalKind, Question, QuestionOption};

fn approval(gate: u64, choices: Vec<ApprovalDecision>) -> ApprovalPrompt {
    ApprovalPrompt {
        gate_id: gate,
        action: "shell.exec".into(),
        detail: Some("x".repeat(DETAIL_CHARS + 10)),
        escalated: false,
        kind: ApprovalKind::Action,
        choices,
        folder: None,
        path: None,
    }
}

fn decided(step: Option<Step>) -> Option<(u64, ApprovalDecision)> {
    match step? {
        Step::Approve { gate, decision } => Some((gate, decision)),
        _ => None,
    }
}

#[test]
fn only_an_offered_choice_of_a_waiting_approval_is_sent_and_only_once() {
    use ApprovalDecision as D;
    let mut prompts = Prompts::default();
    let ask = prompts
        .approval(&approval(7, vec![D::Once, D::Deny]))
        .unwrap();
    assert!(
        prompts
            .approval(&approval(7, vec![D::Once, D::Deny]))
            .is_none(),
        "offered once, even when listed again"
    );
    assert_eq!(
        ask.buttons
            .iter()
            .map(|b| b.label.as_str())
            .collect::<Vec<_>>(),
        ["Yes, allow once", "No, deny"]
    );
    assert!(ask.text.ends_with("… 10 more characters"));

    // Only two choices were offered, so a forged third finds nothing.
    assert!(decided(prompts.pressed("a.7.2")).is_none());
    assert!(
        decided(prompts.pressed("a.8.0")).is_none(),
        "no such approval"
    );
    assert!(decided(prompts.pressed("a.7")).is_none());
    assert!(decided(prompts.pressed("a.7.x")).is_none());
    assert!(matches!(
        decided(prompts.pressed("a.7.1")),
        Some((7, D::Deny))
    ));
    assert!(
        decided(prompts.pressed("a.7.0")).is_none(),
        "a second press changes nothing"
    );
    assert!(
        !prompts.resolved(7),
        "answered here, so nothing more to say"
    );
    assert!(!prompts.waiting());
}

#[test]
fn a_number_answers_the_oldest_approval_and_one_settled_elsewhere_is_told() {
    use ApprovalDecision as D;
    let mut prompts = Prompts::default();
    prompts.approval(&approval(3, vec![D::Once, D::Always, D::Deny]));
    prompts.approval(&approval(4, vec![D::Once, D::Deny]));
    assert!(
        prompts.reply("please go ahead").is_none(),
        "words go to the model"
    );
    assert!(prompts.reply("0").is_none());
    assert!(matches!(
        decided(prompts.reply(" 2 ")),
        Some((3, D::Always))
    ));
    assert!(matches!(decided(prompts.reply("2")), Some((4, D::Deny))));
    let mut elsewhere = Prompts::default();
    elsewhere.approval(&approval(5, vec![D::Once, D::Deny]));
    assert!(elsewhere.resolved(5));
}

fn question(multi: bool) -> Question {
    let option = |label: &str| QuestionOption {
        label: label.into(),
        description: String::new(),
        recommended: false,
    };
    Question {
        prompt: "Which?".into(),
        header: "Pick".into(),
        multi_select: multi,
        options: vec![option("red"), option("blue"), option("green")],
    }
}

#[test]
fn several_questions_are_asked_one_by_one_and_answered_together() {
    let mut prompts = Prompts::default();
    let first = prompts
        .question(QuestionPrompt {
            question_id: 9,
            questions: vec![question(false), question(true)],
        })
        .unwrap();
    assert_eq!(first.buttons.len(), 3);
    assert!(prompts.pressed("q.9.5").is_none(), "no such option");
    let Some(Step::Ask(second)) = prompts.pressed("q.9.1") else {
        panic!("expected the next question")
    };
    assert!(second.buttons.is_empty() && second.text.contains("3. green"));
    let Some(Step::Answer(answer)) = prompts.reply("1,4") else {
        panic!("expected the answer")
    };
    assert_eq!(answer.answers[0].selected, ["blue"]);
    assert_eq!(
        answer.answers[1].other.as_deref(),
        Some("1,4"),
        "4 is no option, so it is words"
    );
}

#[test]
fn numbers_pick_options_and_anything_else_is_a_written_answer() {
    let mut prompts = Prompts::default();
    prompts.question(QuestionPrompt {
        question_id: 9,
        questions: vec![question(true)],
    });
    let Some(Step::Answer(answer)) = prompts.reply("3, 1 3") else {
        panic!("expected the answer")
    };
    assert_eq!(
        (answer.question_id, answer.answers[0].selected.clone()),
        (9, vec!["green".to_owned(), "red".to_owned()])
    );

    prompts.question(QuestionPrompt {
        question_id: 10,
        questions: vec![question(false)],
    });
    let Some(Step::Answer(answer)) = prompts.reply("1,2") else {
        panic!("expected the answer")
    };
    assert_eq!(
        answer.answers[0].other.as_deref(),
        Some("1,2"),
        "one choice only, so it is words"
    );

    prompts.question(QuestionPrompt {
        question_id: 11,
        questions: vec![question(false)],
    });
    let Some(Step::Answer(answer)) = prompts.reply("purple, please") else {
        panic!("expected the answer")
    };
    assert!(answer.answers[0].selected.is_empty());
    assert!(prompts.reply("2").is_none(), "nothing waits any more");
}
