use super::First;
use crate::provider::ProviderError;
use std::time::Duration;

fn status(code: u16, body: &str) -> ProviderError {
    ProviderError::Status(code, body.into())
}

/// Every kind of failure, as a server sends it, has one row: what is tried on
/// the same model first, and whether a model the user listed may then take over.
#[test]
fn each_kind_of_failure_has_its_own_recovery() {
    let throttled = ProviderError::Throttled {
        status: 429,
        retry_after: Duration::from_secs(3),
        body: "slow down".into(),
    };
    let overflow = "This model's maximum context length is 8192 tokens";
    let output = "max_tokens is too large: 9000. available_tokens: 512";
    let rows: Vec<(ProviderError, First, Option<&str>)> = vec![
        // The request itself must change: another model would be sent the same fault.
        (status(400, overflow), First::Compact, None),
        (status(400, output), First::LessOutput, None),
        (
            status(413, "request entity too large"),
            First::LessImage,
            None,
        ),
        (
            status(400, "image exceeds 5 MB maximum"),
            First::LessImage,
            None,
        ),
        (
            status(400, "this model does not support image input"),
            First::WithoutImage,
            None,
        ),
        (
            ProviderError::invalid_tool_call("edit", "call-1", "arguments were not JSON"),
            First::Correct,
            None,
        ),
        (ProviderError::Truncated, First::Continue, None),
        // Worth another try on the same model, then another model's turn.
        (throttled, First::Retry, Some("it is still rate limited")),
        (
            status(429, "rate limit"),
            First::Retry,
            Some("it is still rate limited"),
        ),
        (
            status(503, "overloaded"),
            First::Retry,
            Some("its server kept failing"),
        ),
        (
            ProviderError::Transport("connection reset".into()),
            First::Retry,
            Some("its server could not be reached"),
        ),
        (
            ProviderError::Stream("unexpected end of stream".into()),
            First::Retry,
            Some("its server could not be reached"),
        ),
        (
            ProviderError::Response(r#"{"error":{"type":"overloaded_error"}}"#.into()),
            First::Retry,
            Some("its server kept failing"),
        ),
        (
            ProviderError::StreamStalled { waited_secs: 180 },
            First::Reconnect,
            Some("it stopped sending"),
        ),
        // The same request would get the same answer here; another model may not.
        (
            status(401, "invalid api key"),
            First::Nothing,
            Some("its key was refused"),
        ),
        (
            status(403, "forbidden"),
            First::Nothing,
            Some("its key was refused"),
        ),
        (
            status(402, "insufficient credits"),
            First::Nothing,
            Some("its account has no credit"),
        ),
        (
            status(404, "model not found"),
            First::Nothing,
            Some("the model was not found"),
        ),
        (
            status(400, "unknown parameter: top_k"),
            First::Nothing,
            Some("its server refused the request"),
        ),
        (
            ProviderError::ResponseTimeout { waited_secs: 120 },
            First::Nothing,
            Some("it did not answer in time"),
        ),
        (
            ProviderError::ProgressTimeout { waited_secs: 120 },
            First::Nothing,
            Some("it did not answer in time"),
        ),
        (
            ProviderError::Refused("the model declined to answer: no".into()),
            First::Nothing,
            Some("it declined to answer"),
        ),
        // One thing in the request is left out, once, before anything else.
        (
            status(
                400,
                "Reasoning is mandatory for this endpoint and cannot be disabled",
            ),
            First::DefaultReasoning,
            Some("its server refused the request"),
        ),
        (
            status(400, "error parsing grammar: unknown escape"),
            First::PlainSchemas,
            Some("its server refused the request"),
        ),
        (
            status(400, "Your request was flagged by our safety system"),
            First::Nothing,
            Some("it declined to answer"),
        ),
        // Refused for a reason the words should name, not as a bad key.
        (
            status(403, "<title>Attention Required! | Cloudflare</title>"),
            First::Nothing,
            Some("something in front of its server blocked the request"),
        ),
        (
            status(404, "No endpoints found matching your data policy"),
            First::Nothing,
            Some("the account's data rules leave no provider for it"),
        ),
        (
            ProviderError::Certificate("the certificate could not be verified".into()),
            First::Nothing,
            Some("its security certificate could not be verified"),
        ),
        (
            ProviderError::Decode("no choices in response".into()),
            First::Nothing,
            Some("its reply could not be read"),
        ),
        (
            ProviderError::Response("invalid request".into()),
            First::Nothing,
            Some("its server answered with an error"),
        ),
    ];
    for (error, first, stand_in) in rows {
        let row = error.recovery();
        assert_eq!((row.first, row.stand_in), (first, stand_in), "{error}");
        // Only what is worth sending again is ever sent again.
        let again = matches!(first, First::Retry | First::Reconnect);
        assert_eq!(error.is_retryable(), again, "{error}");
    }
}

/// Which failures are handed over early, which rest the model, and which are
/// waited for: a limit is none of a server being away, and a silent stream
/// keeps its own smaller allowance.
#[test]
fn limits_rest_the_model_and_only_an_absent_server_is_waited_for() {
    let silent = ProviderError::StreamStalled { waited_secs: 180 };
    let rows = [
        (status(429, "rate limit"), Some(0), true, false),
        (status(402, "no credit"), None, true, false),
        (status(503, "overloaded"), Some(2), false, true),
        (status(500, "server error"), None, false, true),
        (
            ProviderError::Transport("reset".into()),
            Some(2),
            false,
            true,
        ),
        (silent, None, false, false),
        (status(401, "bad key"), None, false, false),
        // Worth another try, but not a server to sit and wait for.
        (
            ProviderError::Garbled("unreadable".into()),
            None,
            false,
            false,
        ),
    ];
    let garbled = ProviderError::Garbled("unreadable".into()).recovery();
    assert_eq!((garbled.first, garbled.tries), (First::Retry, Some(3)));
    assert_eq!(status(500, "server error").recovery().tries, None);
    for (error, hand_over_after, rests, comes_back) in rows {
        let row = error.recovery();
        assert_eq!(
            (row.hand_over_after, row.rests, row.comes_back),
            (hand_over_after, rests, comes_back),
            "{error}"
        );
    }
    // 15, 30, then 60 seconds; the server's own wait is used, up to two minutes.
    let waits: Vec<u64> = (1..=5)
        .map(|round| super::wait_for_server(round, None, 0).as_secs())
        .collect();
    assert_eq!(waits, [15, 30, 60, 60, 60]);
    assert_eq!(super::wait_for_server(1, None, 3_000).as_secs(), 18);
    let asked = Some(Duration::from_secs(300));
    assert_eq!(super::wait_for_server(1, asked, 0).as_secs(), 120);
}
