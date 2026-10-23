use super::*;

fn filter(allow: &[&str], deny: &[&str]) -> ToolFilter {
    ToolFilter {
        allow: allow.iter().map(|s| s.to_string()).collect(),
        deny: deny.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn deny_wins_even_over_a_narrower_allow() {
    let rule = filter(&["mcp__db__*", "read"], &["mcp__db__drop_table"]);
    assert!(rule.admits("mcp__db__query"));
    assert!(rule.admits("read"));
    assert!(!rule.admits("mcp__db__drop_table"));
    assert!(
        !rule.admits("write"),
        "an allow list keeps only what it names"
    );
}

#[test]
fn a_pattern_that_cannot_mean_what_it_says_is_refused() {
    for bad in ["", "we b", "*eb", "lsp*start", "**"] {
        assert!(filter(&[], &[bad]).validate().is_err(), "accepted {bad:?}");
    }
    for good in ["*", "lsp*", "task.*", "mcp__github__*", "web"] {
        assert!(filter(&[good], &[]).validate().is_ok(), "refused {good:?}");
    }
}
