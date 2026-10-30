use super::*;

fn html(source: &str) -> String {
    to_telegram(source).0
}

#[test]
fn the_models_markup_cannot_reach_telegram_and_only_web_links_work() {
    assert_eq!(
        html("a < b && <script>alert(1)</script>"),
        "a &lt; b &amp;&amp; &lt;script&gt;alert(1)&lt;/script&gt;"
    );
    assert_eq!(html("<b>not bold</b>"), "&lt;b&gt;not bold&lt;/b&gt;");
    assert_eq!(
        html("[docs](https://x.dev/a?b=1&c=\"2\") [run](javascript:alert(1)) [file](src/main.rs)"),
        "<a href=\"https://x.dev/a?b=1&amp;c=&quot;2&quot;\">docs</a> run file"
    );
}

#[test]
fn markdown_reads_on_a_phone_as_it_does_on_the_computer() {
    assert_eq!(
        html("# Plan\n\nSome **bold**, *italic*, ~~old~~ and `a<b>`.\n\n> quoted\n> > inner"),
        "<b>Plan</b>\n\nSome <b>bold</b>, <i>italic</i>, <s>old</s> and <code>a&lt;b&gt;</code>.\n\n<blockquote>quoted\n\ninner</blockquote>"
    );
    assert_eq!(
        html("```rust\nfn a<T>() {}\n```\nafter"),
        "<pre><code class=\"language-rust\">fn a&lt;T&gt;() {}</code></pre>\n\nafter"
    );
    assert_eq!(
        html("Steps:\n\n1. one\n2. two\n   - inner\n- [x] done\n- [ ] open"),
        "Steps:\n\n1. one\n2. two\n   • inner\n\n☑ done\n☐ open"
    );
    // A diff's lines stay as written inside a block, never becoming a list.
    assert_eq!(
        html("```\n- old\n+ new\n```"),
        "<pre><code>- old\n+ new</code></pre>"
    );
}

#[test]
fn a_table_becomes_rows_a_phone_can_read_and_is_counted_as_shown() {
    let (html, visible) = to_telegram(
        "| Crate | Tests | Result |\n|---|---|---|\n| gateway | 42 | **pass** |\n| tui | 7 | fail |",
    );
    assert_eq!(
        html,
        "<b>gateway</b>\n• Tests: 42\n• Result: <b>pass</b>\n\n<b>tui</b>\n• Tests: 7\n• Result: fail"
    );
    assert_eq!(
        visible,
        "gateway\n• Tests: 42\n• Result: pass\n\ntui\n• Tests: 7\n• Result: fail"
            .encode_utf16()
            .count()
    );
}

#[test]
fn the_count_is_what_telegram_counts_after_reading_the_tags() {
    let (html, visible) = to_telegram("**a** & `<b>` [x](https://e.com)\n\n- é 😀");
    assert_eq!(
        html,
        "<b>a</b> &amp; <code>&lt;b&gt;</code> <a href=\"https://e.com\">x</a>\n\n• é 😀"
    );
    assert_eq!(visible, "a & <b> x\n\n• é 😀".encode_utf16().count());
}

#[test]
fn quoted_text_shows_exactly_as_written() {
    let tricky = "rm *.tmp `x` _y_ <z> [a](b) # c";
    assert_eq!(
        to_telegram(&literal(tricky)).0,
        "rm *.tmp `x` _y_ &lt;z&gt; [a](b) # c"
    );
    assert_eq!(to_telegram(&code("a `b` c")).0, "<code>a `b` c</code>");
    assert_eq!(
        to_telegram(&code_block("```\nfenced\n```")).0,
        "<pre><code>```\nfenced\n```</code></pre>"
    );
}
