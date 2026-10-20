//! ChatGPT sign-ins: which accounts this install holds, their tokens in the credential store,
//! and a fresh access token for each request. Only a person's action may start a sign-in.

use crate::config;
use anyhow::{Context, Result, anyhow, bail};
use providers::chatgpt::{self, Attempt, Callback, Grant, Issuer, Returning, Tokens};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

/// Renewed in the background once less than this is left.
const RENEW_SOON: u64 = 10 * 60;
/// A request waits for a renewal once less than this is left.
const RENEW_NOW: u64 = 5 * 60;
/// Revocation tries before signing out locally anyway.
const REVOKE_TRIES: u32 = 3;

/// The secret-free index kept in `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    /// This install's opaque id, made once before its first sign-in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<Registration>,
    /// The issued client id of the account in use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
}

impl Registry {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// One ChatGPT account registered by this install; signing out keeps it for a later sign-in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    pub client_id: String,
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

/// What a surface shows for one account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    pub client_id: String,
    pub email: Option<String>,
    pub active: bool,
    pub signed_in: bool,
    pub plan_enabled: bool,
}

/// The tokens of one account as the credential store holds them.
#[derive(Serialize, Deserialize)]
struct Saved {
    id_token: String,
    access_token: String,
    refresh_token: String,
    expires_at: u64,
    #[serde(default)]
    earliest_refresh_at: Option<u64>,
    scopes: Vec<String>,
}

impl Saved {
    fn tokens(&self) -> Tokens {
        Tokens {
            access_token: self.access_token.clone(),
            refresh_token: self.refresh_token.clone(),
            expires_at: self.expires_at,
            earliest_refresh_at: self.earliest_refresh_at,
            scopes: self.scopes.clone(),
        }
    }
}

fn credential_id(client_id: &str) -> String {
    format!("chatgpt://{client_id}")
}

/// One account's token in memory, so a request never waits on the keychain.
#[derive(Default)]
struct Live {
    tokens: Mutex<Option<Tokens>>,
    renewing: Arc<tokio::sync::Mutex<()>>,
}

struct Inner {
    issuer_url: String,
    http: chatgpt::Http,
    issuer: tokio::sync::OnceCell<Issuer>,
    live: Mutex<HashMap<String, Arc<Live>>>,
}

/// ChatGPT accounts against one issuer; cheap to clone, shared by every chat.
#[derive(Clone)]
pub struct Accounts(Arc<Inner>);

static OPENAI: LazyLock<Accounts> = LazyLock::new(|| Accounts::at(&issuer_url()));

/// OpenAI, unless `MEDHA_CHATGPT_ISSUER` names a sign-in server on this machine, as tests do.
fn issuer_url() -> String {
    std::env::var("MEDHA_CHATGPT_ISSUER")
        .ok()
        .filter(|url| url.starts_with("http://127.0.0.1:"))
        .unwrap_or_else(|| chatgpt::ISSUER.to_owned())
}

/// A sign-in waiting for the browser; dropping it closes the listener and frees the port.
pub struct SignIn {
    accounts: Accounts,
    attempt: Attempt,
    issuer: Issuer,
    listener: loopback::Listener,
    link: String,
    again: Option<Registration>,
}

impl Accounts {
    pub fn openai() -> &'static Self {
        &OPENAI
    }

    pub fn at(issuer_url: &str) -> Self {
        Self(Arc::new(Inner {
            issuer_url: issuer_url.trim_end_matches('/').to_owned(),
            http: chatgpt::http(),
            issuer: tokio::sync::OnceCell::new(),
            live: Mutex::default(),
        }))
    }

    async fn issuer(&self) -> Result<Issuer> {
        let inner = &self.0;
        Ok(inner
            .issuer
            .get_or_try_init(|| Issuer::discover(&inner.http, &inner.issuer_url))
            .await?
            .clone())
    }

    fn live(&self, client_id: &str) -> Arc<Live> {
        self.0
            .live
            .lock()
            .unwrap()
            .entry(client_id.to_owned())
            .or_default()
            .clone()
    }

    /// Open a sign-in: a new account, or `again` signing in a registered one;
    /// `consent` asks again for plan use declined before.
    pub async fn begin(&self, again: Option<&str>, consent: bool) -> Result<SignIn> {
        let registry = registry()?;
        let again = again
            .map(|client_id| {
                registry
                    .accounts
                    .iter()
                    .find(|account| account.client_id == client_id)
                    .cloned()
                    .ok_or_else(|| anyhow!("No ChatGPT account is registered as {client_id}"))
            })
            .transpose()?;
        let hint = again.as_ref().and_then(|account| saved(&account.client_id));
        let returning = again.as_ref().map(|account| Returning {
            client_id: &account.client_id,
            id_token_hint: hint.as_ref().map(|saved| saved.id_token.as_str()),
            login_hint: account.email.as_deref(),
            consent,
        });
        self.open(returning, again.clone()).await
    }

    /// Authorize again with the client ID a registration was just issued, after OpenAI turned
    /// down its code swap; its sign-in guide says to recover this way rather than register anew.
    pub async fn begin_issued(&self, client_id: &str) -> Result<SignIn> {
        let returning = Returning {
            client_id,
            id_token_hint: None,
            login_hint: None,
            consent: false,
        };
        self.open(Some(returning), None).await
    }

    async fn open(
        &self,
        returning: Option<Returning<'_>>,
        again: Option<Registration>,
    ) -> Result<SignIn> {
        let host_id = host_id()?;
        let issuer = self.issuer().await?;
        let listener = loopback::bind(Some(chatgpt::PREFERRED_PORT))
            .await
            .context("could not open the sign-in listener")?;
        let (attempt, link) = Attempt::begin(&issuer, listener.port(), &host_id, returning)?;
        Ok(SignIn {
            accounts: self.clone(),
            attempt,
            issuer,
            listener,
            link,
            again,
        })
    }

    pub fn list(&self) -> Result<Vec<Status>> {
        let registry = registry()?;
        Ok(registry
            .accounts
            .iter()
            .map(|account| {
                let tokens = saved(&account.client_id);
                Status {
                    client_id: account.client_id.clone(),
                    email: account.email.clone(),
                    active: registry.active.as_deref() == Some(account.client_id.as_str()),
                    signed_in: tokens.is_some(),
                    plan_enabled: tokens.is_some_and(|saved| saved.tokens().plan_enabled()),
                }
            })
            .collect())
    }

    pub fn switch(&self, client_id: &str) -> Result<()> {
        config::edit(|cfg| {
            if !cfg
                .chatgpt
                .accounts
                .iter()
                .any(|account| account.client_id == client_id)
            {
                bail!("No ChatGPT account is registered as {client_id}");
            }
            cfg.chatgpt.active = Some(client_id.to_owned());
            Ok(())
        })
    }

    /// End the account's session at OpenAI, then forget its tokens; the registration stays.
    /// Returns whether OpenAI confirmed the revocation.
    pub async fn sign_out(&self, client_id: &str) -> Result<bool> {
        let mut revoked = true;
        if let Some(saved) = saved(client_id) {
            let issuer = self.issuer().await?;
            revoked = false;
            for attempt in 0..REVOKE_TRIES {
                match chatgpt::revoke(&self.0.http, &issuer, client_id, &saved.refresh_token).await
                {
                    Ok(()) => {
                        revoked = true;
                        break;
                    }
                    Err(chatgpt::Error::Temporary(_)) if attempt + 1 < REVOKE_TRIES => {
                        tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                    }
                    Err(_) => break,
                }
            }
        }
        self.forget(client_id)?;
        config::edit(|cfg| {
            if cfg.chatgpt.active.as_deref() == Some(client_id) {
                cfg.chatgpt.active = None;
            }
            Ok(())
        })?;
        Ok(revoked)
    }

    fn forget(&self, client_id: &str) -> Result<()> {
        self.0.live.lock().unwrap().remove(client_id);
        config::purge_credential(&credential_id(client_id))
    }

    /// A usable access token: from memory, renewed in the background when it is getting old,
    /// and renewed before answering only when it is nearly or already expired.
    pub async fn access_token(&self, client_id: &str) -> Result<String> {
        let live = self.live(client_id);
        let held = live.tokens.lock().unwrap().clone();
        let tokens = match held {
            Some(tokens) => tokens,
            None => {
                let tokens = saved(client_id)
                    .map(|saved| saved.tokens())
                    .ok_or_else(|| anyhow!("Sign in to ChatGPT to use this model."))?;
                *live.tokens.lock().unwrap() = Some(tokens.clone());
                tokens
            }
        };
        let left = tokens.expires_at.saturating_sub(chatgpt::now());
        let allowed = tokens
            .earliest_refresh_at
            .is_none_or(|at| chatgpt::now() >= at);
        if left > RENEW_SOON || (left > RENEW_NOW && !allowed) {
            return Ok(tokens.access_token);
        }
        if left > RENEW_NOW {
            if let Ok(guard) = live.renewing.clone().try_lock_owned() {
                let (accounts, client_id) = (self.clone(), client_id.to_owned());
                tokio::spawn(async move {
                    let _renewing = guard;
                    if let Err(error) = accounts.renew(&client_id).await {
                        tracing::warn!(target: "medha_chatgpt", %error, "background renewal failed");
                    }
                });
            }
            return Ok(tokens.access_token);
        }
        let _renewing = live.renewing.lock().await;
        // Another chat may have renewed it while this one waited.
        if let Some(current) = live.tokens.lock().unwrap().clone()
            && current.expires_at.saturating_sub(chatgpt::now()) > RENEW_NOW
        {
            return Ok(current.access_token);
        }
        match self.renew(client_id).await {
            Ok(access) => Ok(access),
            Err(error) if left > 0 && !error.is::<SignInAgain>() => {
                tracing::warn!(target: "medha_chatgpt", %error, "renewal failed; using the current token");
                Ok(tokens.access_token)
            }
            Err(error) => Err(error),
        }
    }

    /// OpenAI refused the token in memory: renew it, unless another chat already has.
    pub async fn after_refusal(&self, client_id: &str) -> Result<String> {
        let live = self.live(client_id);
        let refused = live
            .tokens
            .lock()
            .unwrap()
            .clone()
            .map(|tokens| tokens.access_token);
        let _renewing = live.renewing.lock().await;
        let current = live
            .tokens
            .lock()
            .unwrap()
            .clone()
            .map(|tokens| tokens.access_token);
        if let Some(current) = current.filter(|current| Some(current) != refused.as_ref()) {
            return Ok(current);
        }
        self.renew_from(client_id, refused.as_deref()).await
    }

    /// Renew once, under the account's renewal lock held by the caller.
    async fn renew(&self, client_id: &str) -> Result<String> {
        self.renew_from(client_id, None).await
    }

    /// `refused` is a token OpenAI turned down, which a saved copy must not hand back.
    async fn renew_from(&self, client_id: &str, refused: Option<&str>) -> Result<String> {
        let saved =
            saved(client_id).ok_or_else(|| anyhow!("Sign in to ChatGPT to use this model."))?;
        let live = self.live(client_id);
        // A newer token saved by another Medha process is used, not renewed again.
        if saved.expires_at.saturating_sub(chatgpt::now()) > RENEW_SOON
            && Some(saved.access_token.as_str()) != refused
        {
            *live.tokens.lock().unwrap() = Some(saved.tokens());
            return Ok(saved.access_token);
        }
        let issuer = self.issuer().await?;
        match chatgpt::refresh(&self.0.http, &issuer, client_id, &saved.refresh_token).await {
            Ok(tokens) => {
                store(client_id, &saved.id_token, &tokens)?;
                *live.tokens.lock().unwrap() = Some(tokens.clone());
                Ok(tokens.access_token)
            }
            Err(chatgpt::Error::SignInAgain(why)) => {
                self.forget(client_id)?;
                Err(anyhow!(SignInAgain(why)))
            }
            Err(error) => Err(error.into()),
        }
    }
}

/// The grant is gone; only a new sign-in recovers.
#[derive(Debug)]
pub struct SignInAgain(pub String);

impl std::fmt::Display for SignInAgain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Sign in to ChatGPT again: {}", self.0)
    }
}

impl std::error::Error for SignInAgain {}

/// OpenAI turned down a new registration's code swap; authorize again as this issued client ID.
#[derive(Debug)]
pub struct Reauthorize(pub String);

impl std::fmt::Display for Reauthorize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChatGPT sign-in needs one more step in the browser.")
    }
}

impl std::error::Error for Reauthorize {}

impl SignIn {
    /// Where to send the person; show it as well as opening it, for machines with no browser.
    pub fn link(&self) -> &str {
        &self.link
    }

    /// Wait for the browser, check everything, then keep the account and make it the active one.
    pub async fn finish(self, within: Duration) -> Result<Status> {
        let redirect = tokio::time::timeout(within, self.listener.redirect())
            .await
            .map_err(|_| anyhow!("The browser did not come back from ChatGPT sign-in in time."))?
            .context("the sign-in listener failed")?;
        let callback = Callback {
            state: redirect.get("state"),
            code: redirect.get("code"),
            error: redirect.get("error"),
            client_id: redirect.get("client_id"),
        };
        let accounts = &self.accounts;
        let grant = match self
            .attempt
            .finish(&accounts.0.http, &self.issuer, callback)
            .await
        {
            Err(chatgpt::Error::Reauthorize(client_id)) => bail!(Reauthorize(client_id)),
            other => other?,
        };
        if let Some(again) = &self.again
            && again.subject != grant.subject
        {
            bail!("That browser signed in to a different ChatGPT account; nothing was changed.");
        }
        keep(&grant)?;
        *accounts.live(&grant.client_id).tokens.lock().unwrap() = Some(grant.tokens.clone());
        Ok(Status {
            client_id: grant.client_id,
            email: grant.email,
            active: true,
            signed_in: true,
            plan_enabled: grant.tokens.plan_enabled(),
        })
    }
}

/// Let every model on a ChatGPT plan ask the active account for its token; later calls do nothing.
pub fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        providers::chatgpt::use_tokens(Arc::new(|refused| {
            Box::pin(async move {
                let active = registry()
                    .ok()
                    .and_then(|registry| registry.active)
                    .ok_or("Sign in to ChatGPT to use this model.")?;
                let accounts = Accounts::openai();
                let token = match refused {
                    true => accounts.after_refusal(&active).await,
                    false => accounts.access_token(&active).await,
                };
                token.map_err(|error| error.to_string())
            })
        }));
    });
}

fn registry() -> Result<Registry> {
    Ok(config::load()?.unwrap_or_default().chatgpt)
}

/// This install's host id, made and saved the first time it is needed.
fn host_id() -> Result<String> {
    if let Some(id) = registry()?.host_id {
        return Ok(id);
    }
    let made = chatgpt::new_host_id()?;
    let mut kept = made.clone();
    config::edit(|cfg| {
        // Another process may have saved one first; theirs is kept.
        kept = cfg.chatgpt.host_id.get_or_insert(made).clone();
        Ok(())
    })?;
    Ok(kept)
}

fn saved(client_id: &str) -> Option<Saved> {
    config::load_key(&credential_id(client_id)).and_then(|blob| serde_json::from_str(&blob).ok())
}

fn store(client_id: &str, id_token: &str, tokens: &Tokens) -> Result<()> {
    for secret in [
        &tokens.access_token,
        &tokens.refresh_token,
        &id_token.to_owned(),
    ] {
        kernel::redact::remember(secret);
    }
    let saved = Saved {
        id_token: id_token.to_owned(),
        access_token: tokens.access_token.clone(),
        refresh_token: tokens.refresh_token.clone(),
        expires_at: tokens.expires_at,
        earliest_refresh_at: tokens.earliest_refresh_at,
        scopes: tokens.scopes.clone(),
    };
    config::store_key(&credential_id(client_id), &serde_json::to_string(&saved)?)
}

fn keep(grant: &Grant) -> Result<()> {
    store(&grant.client_id, &grant.id_token, &grant.tokens)?;
    config::edit(|cfg| {
        let registration = Registration {
            client_id: grant.client_id.clone(),
            subject: grant.subject.clone(),
            email: grant.email.clone(),
        };
        let accounts = &mut cfg.chatgpt.accounts;
        match accounts
            .iter_mut()
            .find(|account| account.client_id == grant.client_id)
        {
            Some(existing) => *existing = registration,
            None => accounts.push(registration),
        }
        cfg.chatgpt.active = Some(grant.client_id.clone());
        Ok(())
    })
}
