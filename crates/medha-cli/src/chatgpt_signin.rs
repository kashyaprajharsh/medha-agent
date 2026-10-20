//! Settings calls for ChatGPT sign-in, shared by every surface. Only a person's settings screen
//! reaches them; the surface opens the returned link, and no answer carries a token.

use anyhow::{Result, anyhow};
use runtime::chatgpt_accounts::{Accounts, SignIn};
use serde_json::{Value, json};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

/// How long a sign-in waits for the browser to come back.
const BROWSER: Duration = Duration::from_secs(5 * 60);

/// The one sign-in waiting for its `finish`; a new one or a cancel drops it and frees its port.
static PENDING: Mutex<Option<SignIn>> = Mutex::new(None);
/// Wakes a `finish` that is already waiting when the person cancels.
static CANCELLED: LazyLock<Notify> = LazyLock::new(Notify::new);

pub(crate) async fn handle(method: &str, params: &Value) -> Option<Result<Value>> {
    let accounts = Accounts::openai();
    let client_id = || {
        params["client_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow!("Choose a ChatGPT account"))
    };
    Some(match method {
        "settings.chatgpt.accounts" => accounts
            .list()
            .map(|accounts| json!({ "accounts": accounts })),
        "settings.chatgpt.begin" => begin(accounts, params).await,
        "settings.chatgpt.finish" => finish().await,
        "settings.chatgpt.cancel" => {
            cancel();
            Ok(json!({ "cancelled": true }))
        }
        "settings.chatgpt.switch" => client_id()
            .and_then(|id| accounts.switch(id))
            .map(|()| json!({ "switched": true })),
        "settings.chatgpt.sign_out" => match client_id() {
            Ok(id) => accounts
                .sign_out(id)
                .await
                .map(|revoked| json!({ "signed_out": true, "revoked": revoked })),
            Err(error) => Err(error),
        },
        _ => return None,
    })
}

async fn begin(accounts: &Accounts, params: &Value) -> Result<Value> {
    cancel();
    let again = params["client_id"].as_str().filter(|id| !id.is_empty());
    let sign_in = accounts.begin(again, params["consent"] == true).await?;
    let link = sign_in.link().to_owned();
    *PENDING.lock().unwrap() = Some(sign_in);
    Ok(json!({ "link": link }))
}

async fn finish() -> Result<Value> {
    let sign_in = PENDING
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| anyhow!("Start a ChatGPT sign-in first"))?;
    let cancelled = CANCELLED.notified();
    tokio::select! {
        status = sign_in.finish(BROWSER) => Ok(json!({ "account": status? })),
        () = cancelled => Err(anyhow!("ChatGPT sign-in was cancelled")),
    }
}

fn cancel() {
    PENDING.lock().unwrap().take();
    CANCELLED.notify_waiters();
}
