use super::*;

/// The size advised for one tool call is never more than half of what the
/// model can write in one reply: a model told to send more could not do it.
#[test]
fn split_advice_names_a_size_the_model_can_write() {
    let tools = ["write".to_owned(), "edit".to_owned()];
    assert!(split_advice(&tools, Some(4_096)).contains("under ~2048 tokens"));
    assert!(split_advice(&tools, Some(64_000)).contains("under ~8000 tokens"));
    let unknown = split_advice(&tools, None);
    assert!(
        unknown.contains("under ~8000 tokens") && unknown.contains("(write, edit)"),
        "{unknown}"
    );
}

/// Each retry of a cut-off tool call gets more room than the one that failed,
/// and never more than the model can give: asking past it only earns a refusal.
#[test]
fn a_cut_off_call_is_retried_with_more_room_within_the_models_limit() {
    // 4,096 configured: doubled each round, capped at 32,768 until that is passed.
    let rounds: Vec<u64> = (1..=4)
        .scan(4_096, |sent, round| {
            *sent = more_room(Some(4_096), Some(*sent), round, None);
            Some(*sent)
        })
        .collect();
    assert_eq!(rounds, [8_192, 16_384, 32_768, 65_536]);

    // Nothing configured or sent: start from the assumed allowance.
    assert_eq!(more_room(None, None, 1, None), 8_192);
    // The model's own limit holds, and at the limit nothing more is asked.
    assert_eq!(more_room(Some(8_000), Some(8_000), 2, Some(20_000)), 20_000);
    assert_eq!(
        more_room(Some(8_000), Some(20_000), 3, Some(20_000)),
        20_000
    );
    // A retry never asks for less than the request that was just cut off.
    assert_eq!(more_room(Some(1_000), Some(50_000), 1, None), 50_000);
}
