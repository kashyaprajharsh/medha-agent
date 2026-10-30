use super::*;

fn bot() -> Bot {
    Bot {
        id: 99,
        username: "MedhaBot".into(),
    }
}

#[test]
fn a_reply_that_grows_when_formatted_is_sent_in_more_parts_not_refused() {
    // Each row is short as typed but gains its column names once drawn for a phone.
    let row = "| a | b |\n";
    let mut table = "| first column name | second column name |\n|---|---|\n".to_owned();
    while table.chars().count() < LIMIT - row.len() {
        table.push_str(row);
    }
    assert!(table.chars().count() <= LIMIT);
    assert!(crate::telegram_format::to_telegram(&table).1 > LIMIT);
    let mut parts = Vec::new();
    fitted(&table, &mut parts);
    assert!(parts.len() > 1);
    assert!(
        parts
            .iter()
            .all(|part| crate::telegram_format::to_telegram(part).1 <= LIMIT)
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
