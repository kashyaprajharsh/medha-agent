//! ChatGPT sign-in for every surface: the desktop's `settings.chatgpt.*` and the terminal's
//! `application.chatgpt.*` are two names for these same calls. The surface opens the returned
//! link, and no answer carries a token.

use anyhow::{Result, anyhow};
use runtime::chatgpt_accounts::{Accounts, Reauthorize, SignIn};
use serde_json::{Value, json};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::oneshot;

/// How long a sign-in waits for the browser to come back.
const BROWSER: Duration = Duration::from_secs(5 * 60);
/// A cancel that names no sign-in reaches every one.
const EVERY: u64 = u64::MAX;

/// The number of the newest sign-in begun; only it may wait for the browser.
static NEWEST: AtomicU64 = AtomicU64::new(0);
/// The one sign-in waiting for its `finish`; dropping it frees its port.
static PENDING: Mutex<Option<Pending>> = Mutex::new(None);

struct Pending {
    id: u64,
    sign_in: SignIn,
    /// Already the one retry OpenAI's guide asks for after a turned-down code swap.
    retry: bool,
}
/// The `finish` waiting on the browser, with its sign-in's number; a cancel for it wakes it.
static WAITING: Mutex<Option<(u64, oneshot::Sender<()>)>> = Mutex::new(None);

pub(crate) async fn handle(method: &str, params: &Value) -> Option<Result<Value>> {
    let accounts = Accounts::openai();
    let client_id = || {
        params["client_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow!("Choose a ChatGPT account"))
    };
    let id = params["id"].as_u64();
    let call = method
        .strip_prefix("settings.chatgpt.")
        .or_else(|| method.strip_prefix("application.chatgpt."))?;
    Some(match call {
        "accounts" => accounts
            .list()
            .map(|accounts| json!({ "accounts": accounts })),
        "begin" => {
            let again = params["client_id"].as_str().filter(|id| !id.is_empty());
            let consent = params["consent"] == true;
            begin(|| accounts.begin(again, consent), false).await
        }
        "finish" => finish(id).await,
        "cancel" => {
            cancel(id.unwrap_or(EVERY));
            Ok(json!({ "cancelled": true }))
        }
        "switch" => client_id()
            .and_then(|id| accounts.switch(id))
            .map(|()| json!({ "switched": true })),
        "sign_out" => match client_id() {
            Ok(id) => accounts
                .sign_out(id)
                .await
                .map(|revoked| json!({ "signed_out": true, "revoked": revoked })),
            Err(error) => Err(error),
        },
        _ => return None,
    })
}

async fn begin<F: Future<Output = Result<SignIn>>>(
    open: impl FnOnce() -> F,
    retry: bool,
) -> Result<Value> {
    let id = NEWEST.fetch_add(1, Ordering::SeqCst) + 1;
    cancel(EVERY);
    let sign_in = open().await?;
    // Starting reaches the network, so an older sign-in can return after a newer one began.
    let mut pending = PENDING.lock().unwrap();
    if NEWEST.load(Ordering::SeqCst) != id {
        return Err(anyhow!("A newer ChatGPT sign-in replaced this one"));
    }
    let link = sign_in.link().to_owned();
    *pending = Some(Pending { id, sign_in, retry });
    Ok(json!({ "link": link, "id": id }))
}

async fn finish(id: Option<u64>) -> Result<Value> {
    let Pending { id, sign_in, retry } = {
        let mut pending = PENDING.lock().unwrap();
        match pending.as_ref() {
            Some(held) if id.is_none_or(|id| id == held.id) => pending.take().unwrap(),
            Some(_) => return Err(anyhow!("A newer ChatGPT sign-in replaced this one")),
            None => return Err(anyhow!("Start a ChatGPT sign-in first")),
        }
    };
    let error = match waited(id, sign_in).await {
        Err(error) => error,
        done => return done,
    };
    match error.downcast::<Reauthorize>() {
        // Authorize once more as the issued client; the browser shows only the account picker.
        Ok(Reauthorize(client_id)) if !retry => {
            let accounts = Accounts::openai();
            let next = begin(|| accounts.begin_issued(&client_id), true).await?;
            Ok(json!({ "again": next }))
        }
        // A second refusal is OpenAI's answer, not a step to repeat.
        Ok(_) => Err(anyhow!(
            "OpenAI turned down the sign-in twice. Start again from Continue with ChatGPT."
        )),
        Err(error) => Err(error),
    }
}

/// Wait for the browser to bring this sign-in back, unless it is cancelled first.
async fn waited(id: u64, sign_in: SignIn) -> Result<Value> {
    // A newer finish replaces this one's sender, which wakes this one as cancelled too.
    let (wake, cancelled) = oneshot::channel();
    *WAITING.lock().unwrap() = Some((id, wake));
    let outcome = tokio::select! {
        status = sign_in.finish(BROWSER) => status.map(|status| json!({ "account": status })),
        _ = cancelled => Err(anyhow!("ChatGPT sign-in was cancelled")),
    };
    let mut waiting = WAITING.lock().unwrap();
    if waiting.as_ref().is_some_and(|(held, _)| *held == id) {
        waiting.take();
    }
    outcome
}

/// Drops the named sign-in, or every one, and wakes its `finish`.
fn cancel(id: u64) {
    let named = |held: &u64| id == EVERY || id == *held;
    let mut pending = PENDING.lock().unwrap();
    if pending.as_ref().is_some_and(|held| named(&held.id)) {
        pending.take();
    }
    let mut waiting = WAITING.lock().unwrap();
    if waiting.as_ref().is_some_and(|(held, _)| named(held))
        && let Some((_, wake)) = waiting.take()
    {
        let _ = wake.send(());
    }
}
