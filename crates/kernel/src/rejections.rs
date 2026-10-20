//! What a server's refusal of a request is about, read from its own words.
//! Every check takes the message in lower case.

/// Names servers use for the settings that turn reasoning on, off, up or down.
const REASONING_FIELDS: [&str; 6] = [
    "reasoning_effort",
    "thinking_config",
    "thinking_budget",
    "enable_thinking",
    "reasoning",
    "thinking",
];

/// Said of a reasoning setting that may not be switched off.
const REQUIRED: [&str; 7] = [
    "mandatory",
    "cannot be disabled",
    "can't be disabled",
    "must be enabled",
    "is required",
    "always enabled",
    "cannot be turned off",
];

/// Said of a setting the server does not take at all.
const UNSUPPORTED: [&str; 12] = [
    "unsupported parameter",
    "unsupported_parameter",
    "not supported",
    "does not support",
    "doesn't support",
    "is deprecated for this model",
    "unknown parameter",
    "unrecognized request argument",
    "unrecognized parameter",
    "invalid parameter",
    "extra inputs are not permitted",
    "invalid option: expected one of",
];

/// Said by a provider that will not answer this request under its content rules.
const POLICY: [&str; 11] = [
    "flagged for possible cybersecurity risk",
    "trusted access for cyber",
    "violates our usage policies",
    "violates openai's usage policies",
    "your request was flagged by",
    "prompt was flagged by our safety",
    "responses cannot be generated due to safety",
    "content_filter",
    "responsibleaipolicyviolation",
    "new_sensitive",
    "content exists risk",
];

/// Said to the person when a provider will not answer under its content rules.
pub(crate) const BLOCKED: &str = "The model's provider blocked this request under its content \
    rules. Try rewording your message or removing sensitive attachments, or switch to \
    another model.";

/// Where a reasoning setting is named by itself: not inside a model's name
/// ("kimi-k2-thinking") and not as a word for a kind of model ("reasoning models").
fn names_reasoning_setting(lower: &str) -> Option<std::ops::Range<usize>> {
    let part = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '/');
    REASONING_FIELDS.iter().find_map(|field| {
        lower.match_indices(field).find_map(|(at, _)| {
            let end = at + field.len();
            let before = lower[..at].chars().next_back();
            let after = &lower[end..];
            let alone = !before.is_some_and(part) && !after.chars().next().is_some_and(part);
            let kind_of_model = after.trim_start().starts_with("model");
            (alone && !kind_of_model).then_some(at..end)
        })
    })
}

/// The server refuses the reasoning setting that was sent: either it does not
/// take the setting, or reasoning may not be switched off there. Its own
/// default is the answer to both.
pub(crate) fn reasoning_setting(lower: &str) -> bool {
    let Some(named) = names_reasoning_setting(lower) else {
        return false;
    };
    // A byte offset may fall inside a character: each end moves to a boundary.
    let from = (named.start.saturating_sub(48)..=named.start)
        .find(|at| lower.is_char_boundary(*at))
        .unwrap_or(0);
    let to = (named.end..=(named.end + 96).min(lower.len()))
        .rev()
        .find(|at| lower.is_char_boundary(*at))
        .unwrap_or(lower.len());
    let near = &lower[from..to];
    REQUIRED.iter().any(|said| near.contains(said))
        || near.contains("unsupported")
        || UNSUPPORTED.iter().any(|said| lower.contains(said))
}

/// The server could not open reasoning it sealed earlier: its key rotated, or another
/// gateway wrapped the request. Every wording here was met on a real endpoint.
pub(crate) fn stale_reasoning(lower: &str) -> bool {
    lower.contains("invalid_encrypted_content")
        || lower.contains("could not decrypt the provided encrypted_content")
        || lower.contains("encrypted content could not be decrypted or parsed")
        || (lower.contains("encrypted content for item") && lower.contains("could not be verified"))
        || (lower.contains("encrypted_content") && lower.contains("was not issued to this caller"))
}

/// A local server could not turn a tool's description into its grammar: it
/// takes few of the `pattern` and `format` rules a description may carry.
pub(crate) fn tool_grammar(lower: &str) -> bool {
    // The same outer words also wrap a chat that has no user message in it.
    if lower.contains("no user query found") {
        return false;
    }
    lower.contains("error parsing grammar")
        || lower.contains("json-schema-to-grammar")
        || (lower.contains("unable to generate parser") && lower.contains("template"))
        || (lower.contains("invalid json schema")
            && lower.contains("regex lookaround")
            && lower.contains("not supported"))
}

/// A rate limit that is not this key's: one passed on from a provider further
/// upstream, or a server that is overloaded for everyone. Another key would
/// meet the same answer, so the key in use is not rested for it.
pub(crate) fn not_the_keys_limit(lower: &str) -> bool {
    lower.contains("provider returned error")
        || lower.contains("overloaded")
        || lower.contains("server overload")
        || lower.contains("server_overload")
}

/// A plan's usage limit: it will not lift on a second try, so none is made.
pub(crate) fn plan_limit(lower: &str) -> bool {
    lower.contains("usage_limit_reached")
        || lower.contains("usage limit reached")
        || lower.contains("usage limit has been reached")
        || lower.contains("gousagelimit")
}

/// A firewall or challenge page in front of the server stopped the request
/// before the server saw it. The key is not at fault and is not rested.
pub(crate) fn blocked_on_the_way(lower: &str) -> bool {
    const BLOCK_PAGE: [&str; 10] = [
        "your request was blocked",
        "request blocked",
        "sorry, you have been blocked",
        "enable javascript and cookies to continue",
        "cdn-cgi/challenge-platform",
        "cf-browser-verification",
        "challenge-error-text",
        "__cf_chl",
        "cf-error-details",
        "attention required! | cloudflare",
    ];
    BLOCK_PAGE.iter().any(|said| lower.contains(said))
}

/// The account's own data rules leave no provider that may serve this model.
pub(crate) fn data_rules(lower: &str) -> bool {
    lower.contains("no endpoints available matching your guardrail")
        || lower.contains("no endpoints available matching your data policy")
        || lower.contains("no endpoints found matching your data policy")
}

/// The provider blocked the request under its content rules.
pub(crate) fn content_policy(lower: &str) -> bool {
    POLICY.iter().any(|said| lower.contains(said))
}

#[cfg(test)]
#[path = "rejections_tests.rs"]
mod tests;
