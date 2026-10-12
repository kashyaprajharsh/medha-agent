use crate::test_http::{Response, Server};
use crate::{AuthKind, ProviderClient, ProviderProfile};
use futures::StreamExt;
use kernel::recovery::Rest;
use kernel::{CompiledContext, Message, Provider};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ANSWER: &str =
    r#"{"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;

/// Ask once; the model named in the request that arrived, or why it failed.
async fn ask(client: &ProviderClient) -> Result<(), kernel::ProviderError> {
    let context = CompiledContext {
        model: String::new(),
        messages: vec![Message::user("hello")],
        ordered: None,
        tools: Vec::new(),
    };
    let request = client.prepare_request(&context)?;
    let mut stream = client.stream_prepared(&request).await?;
    while stream.next().await.is_some() {}
    Ok(())
}

fn asked_for(server: &Server) -> Vec<String> {
    let requests = server.requests.lock().unwrap();
    let models = requests.iter().map(|(_, body)| {
        let body: serde_json::Value = serde_json::from_slice(body).unwrap();
        body["model"].as_str().unwrap().to_owned()
    });
    models.collect()
}

/// The user's own model refuses its key: the next model listed takes the
/// request on its own server, and the own model is back for the next turn.
/// The model that failed is passed over, never asked again as its own stand-in,
/// and the list is read when it is needed, so one changed since is the one used.
#[tokio::test]
async fn the_next_listed_model_takes_over_and_the_own_model_comes_back() {
    let refused = Response {
        status: "401 Unauthorized",
        ..Response::json(r#"{"error":{"message":"bad key"}}"#)
    };
    let own = Server::scripted(vec![refused, Response::json(ANSWER)]).await;
    let other = Server::scripted(vec![Response::json(ANSWER)]).await;
    let profile = |server: &Server, model: &str| {
        ProviderProfile::openai_chat(format!("{}/v1", server.url), model, AuthKind::None)
    };
    // The own model is listed too, as when a chat is moved to one of its stand-ins.
    let saved = Arc::new(Mutex::new(vec![(profile(&own, "own"), String::new())]));
    let listed = saved.clone();
    let client = ProviderClient::from_profile(profile(&own, "own"), "")
        .unwrap()
        .with_fallbacks(move || listed.lock().unwrap().clone());

    let error = ask(&client).await.unwrap_err();
    assert_eq!(error.recovery().stand_in, Some("its key was refused"));

    // Added after the chat opened, and still the one that is used.
    let added = (profile(&other, "other"), String::new());
    saved.lock().unwrap().push(added);
    assert!(client.stand_in_left());
    let stood_in = Some(("own".into(), "other".into()));
    assert_eq!(client.fall_back("own", Rest::NextMessage), stood_in);
    assert_eq!(client.standing_in(), stood_in);
    ask(&client).await.unwrap();
    assert_eq!(asked_for(&other), ["other"]);
    assert!(!client.stand_in_left());
    // A second caller whose request to the own model failed meanwhile is told
    // who answers now; the stand-in, which has not failed, is not passed over.
    assert_eq!(client.fall_back("own", Rest::NextMessage), stood_in);
    assert_eq!(client.active_model(), "other");
    assert_eq!(
        client.fall_back("other", Rest::NextMessage),
        None,
        "nothing is left to try"
    );

    assert_eq!(client.restore_primary(), None);
    ask(&client).await.unwrap();
    assert_eq!(asked_for(&own), ["own", "own"]);
    assert_eq!(asked_for(&other).len(), 1);

    // Left after a limit, the own model is rested for as long as its server
    // said: messages in that time stay with the stand-in, and the next goes back.
    let wait = Duration::from_secs(30);
    assert_eq!(client.standing_in(), None);
    assert_eq!(client.fall_back("own", Rest::Limited(Some(wait))), stood_in);
    tokio::time::pause();
    tokio::time::advance(wait - Duration::from_secs(1)).await;
    assert_eq!(client.restore_primary(), stood_in, "still resting");
    tokio::time::advance(Duration::from_secs(2)).await;
    assert_eq!(client.restore_primary(), None, "rested");
    assert_eq!(client.active_model(), "own");
}

/// Two keys saved for one server. When the first is refused, the very next
/// request carries the second, and the first rests for five minutes: until
/// then no message goes back to it, and after that it leads again.
#[tokio::test]
async fn a_refused_key_rests_and_the_next_saved_key_is_sent() {
    let refused = Response {
        status: "401 Unauthorized",
        ..Response::json(r#"{"error":{"message":"bad key"}}"#)
    };
    let server = Server::scripted(vec![refused, Response::json(ANSWER)]).await;
    let profile = ProviderProfile::openai_chat(format!("{}/v1", server.url), "m", AuthKind::Bearer);
    let keys = ["rotation-test-first", "rotation-test-second"].map(str::to_owned);
    let saved = keys.to_vec();
    let client = ProviderClient::from_profile(profile, &keys[0])
        .unwrap()
        .with_keys(move |_| saved.clone());

    assert!(client.another_key());
    ask(&client).await.unwrap_err();
    assert_eq!(client.next_key(401, None), Some((2, 2)));
    ask(&client).await.unwrap();
    let sent: Vec<String> = {
        let requests = server.requests.lock().unwrap();
        requests
            .iter()
            .map(|(head, _)| head.to_lowercase())
            .collect()
    };
    assert!(sent[0].contains("rotation-test-first"), "{sent:?}");
    assert!(sent[1].contains("rotation-test-second"), "{sent:?}");

    assert!(!client.another_key(), "the first key is resting");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(4 * 60)).await;
    client.restore_primary();
    assert!(
        !client.another_key(),
        "a new message does not go back to it yet"
    );
    tokio::time::advance(Duration::from_secs(61)).await;
    client.restore_primary();
    // Back on the first key: a limit on it now leaves the second to turn to.
    assert_eq!(client.next_key(429, None), Some((2, 2)));
}
