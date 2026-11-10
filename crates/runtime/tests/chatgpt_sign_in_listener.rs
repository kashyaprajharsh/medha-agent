//! The sign-in listener against stray local requests, with a fake OpenAI.

use runtime::chatgpt_accounts::Accounts;
use std::collections::HashMap;
use std::time::Duration;
use test_support::fake_openai::{FakeOpenAi, Grant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PLAN: &str = "chatgpt.tokens.use.direct email offline_access openid profile resource.invoke";

fn query(url: &str) -> HashMap<String, String> {
    let url = url.replace("/auth/callback", "/auth/callback/");
    url::Url::parse(&url)
        .unwrap()
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

async fn visit(port: u16, target: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    stream
        .write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut page = String::new();
    let _ = stream.read_to_string(&mut page).await;
    page
}

/// A silent connection and a stray error with someone else's state do not end a sign-in; the real redirect does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stray_local_requests_never_end_a_sign_in() {
    let files = tempfile::tempdir().unwrap();
    // SAFETY: this integration binary has one test, so no other thread reads the environment.
    unsafe {
        std::env::set_var("MEDHA_HOME", files.path().join("home"));
        std::env::set_var("MEDHA_CRED_STORE", "file");
    }
    let fake = FakeOpenAi::start().await;
    let accounts = Accounts::at(&fake.url);
    let attempt = accounts.begin(None, false).await.unwrap();
    let link = query(attempt.link());
    fake.grant(Grant {
        client_id: "oaiapp_1",
        subject: "user-1",
        nonce: &link["nonce"],
        scope: PLAN,
        refresh: "r1",
        expires_in: 3600,
    });
    let port = url::Url::parse(&link["redirect_uri"])
        .unwrap()
        .port()
        .unwrap();
    let browser = async {
        let silent = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        visit(
            port,
            "/auth/callback?error=access_denied&state=someone-else",
        )
        .await;
        let target = format!(
            "/auth/callback?code=code-1&state={}&client_id=oaiapp_1",
            link["state"]
        );
        let page = visit(port, &target).await;
        drop(silent);
        page
    };
    let (status, page) = tokio::join!(attempt.finish(Duration::from_secs(20)), browser);
    assert!(status.unwrap().signed_in);
    assert!(page.contains("Signed in"), "{page}");
}
