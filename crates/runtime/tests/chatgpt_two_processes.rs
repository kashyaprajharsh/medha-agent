//! Two Medha processes sharing one ChatGPT sign-in, against a fake OpenAI.

use runtime::chatgpt_accounts::Accounts;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use test_support::fake_openai::{FakeOpenAi, Grant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PLAN: &str = "chatgpt.tokens.use.direct email offline_access openid profile resource.invoke";
const TOKEN: &str = "POST /api/accounts/oauth/token";

fn query(url: &str) -> HashMap<String, String> {
    let url = url.replace("/auth/callback", "/auth/callback/");
    url::Url::parse(&url)
        .unwrap()
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

async fn browser(redirect_uri: &str, state: &str) {
    let port: u16 = url::Url::parse(redirect_uri).unwrap().port().unwrap();
    let target = format!("/auth/callback?code=code-1&state={state}&client_id=oaiapp_1");
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    stream
        .write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut page = String::new();
    let _ = stream.read_to_string(&mut page).await;
}

/// Two processes renewing one nearly expired sign-in at once refresh it once and both stay signed in.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_processes_renewing_one_sign_in_both_keep_it() {
    let files = tempfile::tempdir().unwrap();
    // SAFETY: this integration binary has one test, so no other thread reads the environment.
    unsafe {
        std::env::set_var("MEDHA_HOME", files.path().join("home"));
        std::env::set_var("MEDHA_CRED_STORE", "file");
    }
    let fake = FakeOpenAi::start().await;
    let first = Accounts::at(&fake.url);
    let attempt = first.begin(None, false).await.unwrap();
    let link = query(attempt.link());
    fake.grant(Grant {
        client_id: "oaiapp_1",
        subject: "user-1",
        nonce: &link["nonce"],
        scope: PLAN,
        refresh: "r1",
        expires_in: 60,
    });
    let (status, ()) = tokio::join!(
        attempt.finish(Duration::from_secs(10)),
        browser(&link["redirect_uri"], &link["state"])
    );
    assert!(status.unwrap().signed_in);

    // A second process has its own memory and its own renewal lock; OpenAI refuses a refresh token used twice.
    let second = Accounts::at(&fake.url);
    fake.token("200 OK", json!({"access_token": "access-r2", "refresh_token": "r2", "expires_in": 3600, "scope": PLAN}));
    fake.token("400 Bad Request", json!({"error": "refresh_token_reused"}));
    let (one, two) = tokio::join!(
        first.access_token("oaiapp_1"),
        second.access_token("oaiapp_1")
    );
    let refreshes = fake
        .seen()
        .iter()
        .filter(|(request, _)| request == TOKEN)
        .count();
    assert_eq!(refreshes, 2, "one sign-in exchange and one renewal");
    assert_eq!(one.unwrap(), "access-r2");
    assert_eq!(two.unwrap(), "access-r2");
    let restarted = Accounts::at(&fake.url);
    assert_eq!(
        restarted.access_token("oaiapp_1").await.unwrap(),
        "access-r2",
        "the account stays signed in"
    );
}
