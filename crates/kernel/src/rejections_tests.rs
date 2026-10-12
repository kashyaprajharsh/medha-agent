use super::*;

/// A refusal is read for what it is about, in the words servers really use, and
/// a model's name or an unrelated complaint is not mistaken for one.
#[test]
fn a_refusal_is_read_for_what_it_is_about() {
    let about_reasoning = [
        "reasoning is mandatory for this endpoint and cannot be disabled",
        "unrecognized request argument supplied: reasoning_effort",
        "reasoning_effort 'none' unsupported; use minimal|low|medium|high|xhigh",
        "this model does not support the thinking field",
    ];
    for said in about_reasoning {
        assert!(reasoning_setting(said), "{said}");
    }
    let not_about_reasoning = [
        "the model kimi-k2-thinking is not supported when using this account",
        "temperature is not supported with reasoning models",
        "unknown parameter: top_k",
        "reasoning took too long",
    ];
    for said in not_about_reasoning {
        assert!(!reasoning_setting(said), "{said}");
    }

    assert!(tool_grammar("error parsing grammar: unknown escape at \\d"));
    assert!(tool_grammar(
        "invalid json schema: regex lookaround is not supported"
    ));
    assert!(tool_grammar("unable to generate parser for this template"));
    assert!(!tool_grammar(
        "unable to generate parser for this template: no user query found"
    ));

    assert!(content_policy(
        "your request was flagged by our safety system"
    ));
    assert!(content_policy(r#"{"code":"content_filter"}"#));

    let upstream = r#"{"error":{"message":"provider returned error","metadata":{"provider_name":"deepinfra"}}}"#;
    assert!(not_the_keys_limit(upstream));
    assert!(not_the_keys_limit("the service is temporarily overloaded"));
    assert!(!not_the_keys_limit("rate limit exceeded for this api key"));
    assert!(plan_limit(r#"{"type":"usage_limit_reached"}"#));
    assert!(!plan_limit("rate limit exceeded, retry in 20s"));
    assert!(!content_policy("invalid api key"));
}
