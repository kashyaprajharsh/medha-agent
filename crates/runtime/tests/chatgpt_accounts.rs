//! ChatGPT accounts against a fake OpenAI: sign-in, storage, renewal and sign-out.

use runtime::chatgpt_accounts::{Accounts, SignInAgain};
use serde_json::json;
use std::collections::HashMap;
use std::time::{Duration, Instant};
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

/// What the browser does: come back to Medha's listener with the code.
async fn browser(redirect_uri: &str, state: &str, client_id: Option<&str>) {
    let port: u16 = url::Url::parse(redirect_uri).unwrap().port().unwrap();
    let mut target = format!("/auth/callback?code=code-1&state={state}");
    if let Some(client_id) = client_id {
        target.push_str(&format!("&client_id={client_id}"));
    }
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

/// Sign in as `subject`, its token living `expires_in` seconds; `again` names a saved registration.
async fn sign_in(
    accounts: &Accounts,
    fake: &FakeOpenAi,
    again: Option<&str>,
    subject: &str,
    refresh: &str,
    expires_in: u64,
) -> anyhow::Result<runtime::chatgpt_accounts::Status> {
    let attempt = accounts.begin(again, false).await?;
    let link = query(attempt.link());
    fake.grant(Grant {
        client_id: "oaiapp_1",
        subject,
        nonce: &link["nonce"],
        scope: PLAN,
        refresh,
        expires_in,
    });
    let issued = again.is_none().then_some("oaiapp_1");
    let (status, ()) = tokio::join!(
        attempt.finish(Duration::from_secs(10)),
        browser(&link["redirect_uri"], &link["state"], issued)
    );
    status
}

/// A model list server that records the authorization each request carried.
async fn models_server() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let headers = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = headers.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut request = vec![0u8; 8192];
            let read = socket.read(&mut request).await.unwrap_or(0);
            let head = String::from_utf8_lossy(&request[..read]).to_ascii_lowercase();
            if let Some(line) = head.lines().find(|line| line.starts_with("authorization:")) {
                seen.lock().unwrap().push(line.trim().to_owned());
            }
            let body = r#"{"models":[{"slug":"gpt-a","visibility":"list"}]}"#;
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(reply.as_bytes()).await;
        }
    });
    (url, headers)
}

fn token_requests(fake: &FakeOpenAi) -> usize {
    fake.seen()
        .iter()
        .filter(|(request, _)| request == TOKEN)
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chatgpt_account_signs_in_renews_once_and_signs_out_without_leaving_secrets_behind() {
    let files = tempfile::tempdir().unwrap();
    let home = files.path().join("home");
    // This integration binary has one test; environment choices belong to its fixture.
    unsafe {
        std::env::set_var("MEDHA_HOME", &home);
        std::env::set_var("MEDHA_CRED_STORE", "file");
    }
    let fake = FakeOpenAi::start().await;
    let accounts = Accounts::at(&fake.url);

    // The install id is made once and reused; a dropped sign-in frees its port.
    let first = accounts.begin(None, false).await.unwrap();
    let link = query(first.link());
    assert_eq!(link["client_id"], "dynamic_agent_client");
    assert!(link["ext_agent_host_id"].starts_with("urn:uuid:"));
    let port = url::Url::parse(&link["redirect_uri"])
        .unwrap()
        .port()
        .unwrap();
    drop(first);
    std::net::TcpListener::bind(("127.0.0.1", port)).expect("the dropped sign-in kept its port");
    let again = query(accounts.begin(None, false).await.unwrap().link());
    assert_eq!(again["ext_agent_host_id"], link["ext_agent_host_id"]);

    // A first sign-in keeps the account and its plan permission; no token reaches config.toml.
    let status = sign_in(&accounts, &fake, None, "user-1", "r1", 3600)
        .await
        .unwrap();
    assert!(status.active && status.signed_in && status.plan_enabled);
    let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(
        config.contains("oaiapp_1") && !config.contains("access-r1") && !config.contains("r1\"")
    );

    // A plan model never picks up the API key saved for the same address; its token is the account's.
    runtime::config::store_key("https://api.openai.com/v1", "sk-proj-saved-api-key").unwrap();
    let (models, headers) = models_server().await;
    let mut plan = providers::ProviderProfile::openai_chat(
        "https://api.openai.com/v1",
        "gpt-a",
        providers::AuthKind::ChatGptPlan,
    );
    plan.protocol = kernel::Protocol::OpenAiResponses;
    assert_eq!(
        runtime::config::deployment_credential(&plan, None).unwrap(),
        ""
    );
    runtime::chatgpt_accounts::install();
    plan.base_url = models;
    providers::discovery::list_models_for_profile(&plan, "")
        .await
        .unwrap();
    assert_eq!(
        headers.lock().unwrap().as_slice(),
        ["authorization: bearer access-r1"]
    );

    // A fresh token is served from memory, and after a restart from the store, without OpenAI.
    let asked = fake.seen().len();
    assert_eq!(
        accounts.access_token("oaiapp_1").await.unwrap(),
        "access-r1"
    );
    let restarted = Accounts::at(&fake.url);
    assert_eq!(
        restarted.access_token("oaiapp_1").await.unwrap(),
        "access-r1"
    );
    assert_eq!(
        fake.seen().len(),
        asked,
        "a fresh token went back to OpenAI"
    );

    // Two chats with a nearly expired token: one renewal, and both get the new token.
    sign_in(&accounts, &fake, Some("oaiapp_1"), "user-1", "r2", 60)
        .await
        .unwrap();
    let before = token_requests(&fake);
    fake.token("200 OK", json!({"access_token": "access-r3", "refresh_token": "r3", "expires_in": 3600, "scope": PLAN}));
    let (one, two) = tokio::join!(
        accounts.access_token("oaiapp_1"),
        accounts.access_token("oaiapp_1")
    );
    assert_eq!(
        (one.unwrap(), two.unwrap()),
        ("access-r3".into(), "access-r3".into())
    );
    assert_eq!(
        token_requests(&fake) - before,
        1,
        "two chats renewed one token twice"
    );
    let renewal = &fake
        .seen()
        .iter()
        .rev()
        .find(|(request, _)| request == TOKEN)
        .unwrap()
        .1
        .clone();
    assert!(renewal.contains("refresh_token=r2") && renewal.contains("client_id=oaiapp_1"));

    // A token getting old is returned at once and renewed behind the request.
    sign_in(&accounts, &fake, Some("oaiapp_1"), "user-1", "r4", 450)
        .await
        .unwrap();
    fake.token("200 OK", json!({"access_token": "access-r5", "refresh_token": "r5", "expires_in": 3600, "scope": PLAN}));
    let started = Instant::now();
    assert_eq!(
        accounts.access_token("oaiapp_1").await.unwrap(),
        "access-r4"
    );
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "the request waited for a renewal"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while accounts.access_token("oaiapp_1").await.unwrap() != "access-r5" {
        assert!(
            Instant::now() < deadline,
            "the background renewal never landed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Signing in again as someone else changes nothing.
    let other = sign_in(&accounts, &fake, Some("oaiapp_1"), "user-2", "r6", 3600).await;
    assert!(
        other
            .unwrap_err()
            .to_string()
            .contains("different ChatGPT account")
    );
    assert_eq!(
        accounts.access_token("oaiapp_1").await.unwrap(),
        "access-r5"
    );

    // A refresh token already used elsewhere: tokens forgotten, the registration kept.
    sign_in(&accounts, &fake, Some("oaiapp_1"), "user-1", "r7", 60)
        .await
        .unwrap();
    fake.token("400 Bad Request", json!({"error": "refresh_token_reused"}));
    let lost = accounts.access_token("oaiapp_1").await.unwrap_err();
    assert!(lost.is::<SignInAgain>(), "{lost}");
    let listed = accounts.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert!(!listed[0].signed_in);

    // Signing out revokes at OpenAI and keeps the registration for a later sign-in.
    sign_in(&accounts, &fake, Some("oaiapp_1"), "user-1", "r8", 3600)
        .await
        .unwrap();
    assert!(accounts.sign_out("oaiapp_1").await.unwrap());
    let revoked = fake
        .seen()
        .into_iter()
        .find(|(request, _)| request.starts_with("POST /api/accounts/oauth/revoke"))
        .unwrap();
    assert!(revoked.1.contains("token=r8") && revoked.1.contains("token_type_hint=refresh_token"));
    let listed = accounts.list().unwrap();
    assert!(!listed[0].signed_in && !listed[0].active);
    let credentials = std::fs::read_to_string(home.join("credentials.toml")).unwrap_or_default();
    assert!(
        !credentials.contains("access-r8"),
        "signing out left the token behind"
    );

    // When OpenAI cannot confirm, the tokens still go and the caller is told.
    sign_in(&accounts, &fake, Some("oaiapp_1"), "user-1", "r9", 3600)
        .await
        .unwrap();
    fake.revoke_answers("503 Service Unavailable");
    assert!(!accounts.sign_out("oaiapp_1").await.unwrap());
    assert!(!accounts.list().unwrap()[0].signed_in);
}
