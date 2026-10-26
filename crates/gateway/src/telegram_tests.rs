use super::*;

fn bot() -> Bot {
    Bot {
        id: 99,
        username: "MedhaBot".into(),
    }
}

#[test]
fn the_models_text_cannot_inject_markup_and_code_survives_whole() {
    assert_eq!(
        html("a < b && <script>"),
        "a &lt; b &amp;&amp; &lt;script&gt;"
    );
    assert_eq!(
        html("run `ls <dir>` now"),
        "run <code>ls &lt;dir&gt;</code> now"
    );
    assert_eq!(html("a ` lone tick"), "a ` lone tick");
    // Unmatched markers are left as typed rather than guessed at.
    assert_eq!(html("**done** and **not"), "**done** and **not");
    assert_eq!(html("**done** twice **ok**"), "<b>done</b> twice <b>ok</b>");
    assert_eq!(
        html("```rust\nfn a<T>() {}\n```\nafter"),
        "<pre><code>fn a&lt;T&gt;() {}\n</code></pre>after"
    );
    assert_eq!(
        html("```\nnever closed"),
        "<pre><code>never closed</code></pre>"
    );
}

#[test]
fn a_group_message_reaches_the_bot_only_when_it_is_named_or_answered() {
    let me = bot();
    let text = "hey @medhabot run it";
    let named = json!({"entities": [{"type": "mention", "offset": 4, "length": 9}]});
    assert!(mentions(&named, text, &me));
    let other = json!({"entities": [{"type": "mention", "offset": 4, "length": 9}]});
    assert!(!mentions(&other, "hey @someoneel run it", &me));
    let command = json!({"entities": [{"type": "bot_command", "offset": 0, "length": 13}]});
    assert!(mentions(&command, "/new@MedhaBot", &me));
    assert!(!mentions(&command, "/new@OtherBot", &me));
    let reply = json!({"reply_to_message": {"from": {"id": 99}}});
    assert!(mentions(&reply, "yes", &me));
    assert!(!mentions(&json!({}), "just chatting", &me));
    // Offsets count UTF-16 units: an emoji before the name takes two.
    let emoji = json!({"entities": [{"type": "mention", "offset": 3, "length": 9}]});
    assert!(mentions(&emoji, "😀 @MedhaBot", &me));
}

#[tokio::test]
async fn a_network_error_never_shows_the_token_in_the_address() {
    let http = client().unwrap();
    let unreachable = http
        .post("http://127.0.0.1:1/bot123:SECRET-TOKEN/getMe")
        .send()
        .await
        .unwrap_err();
    assert!(
        unreachable.to_string().contains("SECRET-TOKEN"),
        "reqwest names the address"
    );
    let shown = format!("{:#}", hidden(unreachable));
    assert!(!shown.contains("SECRET-TOKEN"), "{shown}");
}
