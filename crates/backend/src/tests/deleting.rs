use super::*;

async fn create_tied(client: &mut Tester) -> String {
    let params = json!({"ends_with_client": true, "ending_ms": 300});
    client.ask("session.create", None, params).await["result"]["session"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn folder_request(client: &mut Tester, method: &str, chat: &str) -> Value {
    let request = json!({"method": method, "folder": "/w", "params": {"session": chat}});
    let asked = client.post(request).await;
    client.answer(asked).await
}

/// A delete waits for a chat its last viewer just left to finish ending, and refuses one still watched.
#[tokio::test]
async fn a_delete_waits_for_a_chat_already_ending_and_refuses_one_still_watched() {
    let backend = backend();
    let mut client = connect(&backend);
    let chat = create_tied(&mut client).await;
    client.attach(&chat, None).await;
    let watched = folder_request(&mut client, "chats.delete", &chat).await;
    assert_eq!(watched["error"]["message"], "this chat is open");

    client.ask("session.detach", Some(&chat), json!({})).await;
    let deleted = folder_request(&mut client, "chats.delete", &chat).await;
    assert_eq!(deleted["result"]["held"], true, "{deleted}");

    let other = create_tied(&mut client).await;
    client.attach(&other, None).await;
    client.ask("session.detach", Some(&other), json!({})).await;
    let history = folder_request(&mut client, "folder.history.delete", &other).await;
    assert_eq!(history["result"]["deleted"], true, "{history}");
}
