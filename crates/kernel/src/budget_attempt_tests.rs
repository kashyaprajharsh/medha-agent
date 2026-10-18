use super::*;

fn governor() -> Governor {
    Governor::new(Budget {
        max_tokens: Some(1000),
        ..Budget::default()
    })
}

#[test]
fn proven_non_submission_and_rejection_refund_but_uncertain_work_stays_charged() {
    for (error, expected) in [
        (crate::ProviderError::NotSent("connect".into()), 0),
        (crate::ProviderError::Status(401, "rejected".into()), 0),
        (crate::ProviderError::Status(429, "rejected".into()), 0),
        (
            crate::ProviderError::Transport("connection broke after submission".into()),
            100,
        ),
        (
            crate::ProviderError::Status(500, "upstream may have executed".into()),
            100,
        ),
    ] {
        let mut governor = governor();
        governor
            .reserve_model(Some(80), Some(20), None)
            .unwrap()
            .reconcile_attempt(None, None, Some(&error))
            .unwrap();
        let remaining = governor.reserve_model(Some(1000 - expected), Some(0), None);
        assert!(remaining.is_ok(), "{error}");
        assert_eq!(governor.tokens(), expected);
    }
}

#[test]
fn real_usage_is_settled_even_when_the_attempt_ends_in_an_error() {
    let mut governor = governor();
    let usage = Usage {
        prompt_tokens: 12,
        completion_tokens: 8,
        total_tokens: 20,
        cached_prompt_tokens: None,
        cache_write_tokens: None,
    };
    governor
        .reserve_model(Some(80), Some(20), None)
        .unwrap()
        .reconcile_attempt(
            Some(usage),
            None,
            Some(&crate::ProviderError::Stream("late error".into())),
        )
        .unwrap();
    assert_eq!(governor.tokens(), 20);
}
