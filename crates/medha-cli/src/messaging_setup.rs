//! Messaging setup for every surface: `medha gateway setup`, the TUI's `application.messaging.*`
//! and the desktop's `settings.messaging.*` share these steps. The token stays in this process from
//! the moment it arrives until it is saved; no answer carries it.

use anyhow::{Context, Result, anyhow, bail};
use gateway::one_tap::{Handed, Waiting};
use gateway::pairing::Pairing;
use gateway::platform::{Heard, Platform as _};
use gateway::platforms;
use gateway::telegram::Telegram;
use protocol::{MessagingOverview, MessagingSaved, MessagingStep};
use runtime::config;
use runtime::messaging::{Platform, credential_id};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Each wait answers within this, inside the minute a client gives a quick answer.
const STEP: Duration = Duration::from_secs(40);
/// Kept back from a step for reaching the new bot once it exists.
const CONNECT: Duration = Duration::from_secs(15);
/// How long the owner has to send the new bot a first message.
const FIRST_MESSAGE: Duration = Duration::from_secs(5 * 60);
/// After this a setup nobody finished is dropped, with its token.
const ABANDONED: Duration = Duration::from_secs(15 * 60);
/// The setting naming the bot people message.
const ACCOUNT: &str = "account";
/// The setting saying how the bot was made: `scanned` bots answer their owner only.
const MADE: &str = "made";
const SCANNED: &str = "scanned";
const PAIRED: &str = "Paired. This chat now talks to Medha on your computer.";

/// The creator, as Telegram itself showed them writing to the new bot.
pub(crate) struct Owner {
    pub user: String,
    pub name: String,
    pub chat: String,
}

/// The number of the newest setup begun; only it may advance.
static NEWEST: AtomicU64 = AtomicU64::new(0);
/// The one setup in progress. Replacing or dropping it drops its key and token.
static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
/// One wait at a time; another asker waits its turn within its own step.
static STEPPING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Pending {
    id: u64,
    stage: Stage,
}

#[derive(Clone)]
enum Stage {
    Waiting(Arc<Waiting>),
    /// `telegram` is empty until the new bot is reached, which a slow network can delay a step.
    Created {
        handed: Arc<Handed>,
        telegram: Option<Arc<Telegram>>,
        until: Instant,
    },
    Heard {
        handed: Arc<Handed>,
        telegram: Arc<Telegram>,
        owner: Arc<Owner>,
    },
}

pub(crate) async fn handle(
    method: &str,
    params: &Value,
    workspace: &Path,
) -> Option<Result<Value>> {
    let call = method
        .strip_prefix("settings.messaging.")
        .or_else(|| method.strip_prefix("application.messaging."))?;
    Some(answer(call, params, workspace).await)
}

async fn answer(call: &str, params: &Value, workspace: &Path) -> Result<Value> {
    let id = || params["id"].as_u64().context("Name the setup");
    let text = |key: &str| {
        params[key]
            .as_str()
            .filter(|value| !value.is_empty())
            .with_context(|| format!("{key} is required"))
    };
    let folder = || params["folder"].as_str().map(PathBuf::from);
    Ok(match call {
        "status" => json!(overview()?),
        "begin" => json!(begin(text("platform")?).await?),
        "await" => json!(advance(id()?).await?),
        "save" => json!(save_pending(id()?, folder(), workspace).await?),
        "save_token" => {
            let platform = text("platform")?;
            let token = text("token")?.trim();
            let account = tokio::time::timeout(STEP, platforms::check(platform, token))
                .await
                .map_err(|_| {
                    anyhow!("The platform did not answer. Check the connection and try again.")
                })??;
            let folder = chosen_folder(folder(), workspace)?;
            json!(save(platform, token, &account, &folder, None).await?)
        }
        "cancel" => {
            cancel(id()?);
            json!({ "cancelled": true })
        }
        "approve" => {
            let person = Pairing::open()?
                .approve(text("platform")?, text("code")?.trim())?
                .map_err(|why| anyhow!(why))?;
            json!(protocol::MessagingPerson {
                platform: person.platform,
                user: person.user,
                name: person.name,
            })
        }
        "revoke" => json!({
            "revoked": Pairing::open()?.revoke(text("platform")?, text("user")?)?
        }),
        "folder" => {
            let folder = PathBuf::from(text("folder")?);
            change_folder(serde_json::from_value(params["change"].clone())?, &folder)?;
            json!(overview()?)
        }
        "steps" => {
            let show = params["show"]
                .as_bool()
                .context("Say whether to show steps")?;
            let mut cfg = config::load()?.unwrap_or_default();
            cfg.gateway.steps = show;
            config::save(&cfg)?;
            json!(overview()?)
        }
        "gateway" => {
            match serde_json::from_value(params["action"].clone())? {
                protocol::GatewayAction::Start if crate::gateway_service::describe().is_none() => {
                    crate::gateway_service::install().map(drop)?
                }
                protocol::GatewayAction::Start => crate::gateway_service::restart()?,
                protocol::GatewayAction::Stop => crate::gateway_service::stop()?,
            }
            json!(overview()?)
        }
        _ => bail!("Unknown messaging action"),
    })
}

/// The gateway reads these on each chat it opens, so a change applies without a restart.
pub(crate) fn change_folder(change: protocol::FolderChange, folder: &Path) -> Result<()> {
    let mut cfg = config::load()?.unwrap_or_default();
    let messaging = &mut cfg.gateway;
    match change {
        protocol::FolderChange::Default => {
            messaging.make_default(runtime::messaging::admit_folder(folder)?);
        }
        protocol::FolderChange::Add => {
            let folder = runtime::messaging::admit_folder(folder)?;
            if messaging.folder.is_none() {
                messaging.folder = Some(folder);
            } else if !messaging.allowed().contains(&folder) {
                messaging.folders.push(folder);
            }
        }
        protocol::FolderChange::Remove => {
            // Compared as saved, so a folder deleted from disk can still be taken off the list.
            let folder = folder
                .canonicalize()
                .unwrap_or_else(|_| folder.to_path_buf());
            if messaging.folder.as_ref() == Some(&folder) {
                bail!("New chats start in this folder. Make another folder the default first.");
            }
            messaging.folders.retain(|kept| *kept != folder);
        }
    }
    config::save(&cfg)
}

/// Every platform Medha knows, how far each is set up, and who may talk to the gateway.
pub(crate) fn overview() -> Result<MessagingOverview> {
    let cfg = config::load()?.unwrap_or_default();
    let pairing = Pairing::open()?;
    let platforms = platforms::ALL
        .iter()
        .map(|manifest| {
            let saved = cfg.gateway.platforms.get(manifest.name);
            protocol::MessagingPlatform {
                name: manifest.name.to_owned(),
                label: manifest.label.to_owned(),
                ready: saved.is_some_and(|saved| saved.enabled)
                    && config::load_key(&credential_id(manifest.name)).is_some(),
                account: saved.and_then(|saved| saved.settings.get(ACCOUNT).cloned()),
                home: saved.and_then(Platform::home).is_some(),
                steps: manifest.steps.to_owned(),
                token: manifest.token.to_owned(),
                one_tap: manifest.name == "telegram",
                owner_only: saved.is_some_and(|saved| {
                    saved.settings.get(MADE).map(String::as_str) == Some(SCANNED)
                }),
            }
        })
        .collect();
    Ok(MessagingOverview {
        folders: cfg.gateway.allowed(),
        steps: cfg.gateway.steps,
        folder: cfg.gateway.folder,
        running: gateway::running(),
        installed: crate::gateway_service::describe().is_some(),
        platforms,
        people: pairing
            .approved()?
            .into_iter()
            .map(|person| protocol::MessagingPerson {
                platform: person.platform,
                user: person.user,
                name: person.name,
            })
            .collect(),
        waiting: pairing
            .waiting()?
            .into_iter()
            .map(|request| protocol::MessagingRequest {
                platform: request.platform,
                user: request.user,
                name: request.name,
                minutes: request.minutes,
            })
            .collect(),
    })
}

async fn begin(platform: &str) -> Result<protocol::MessagingSetup> {
    if platform != "telegram" {
        bail!("Only Telegram can be set up by scanning a code");
    }
    let id = NEWEST.fetch_add(1, Ordering::SeqCst) + 1;
    cancel(u64::MAX);
    let waiting = gateway::one_tap::begin(&gateway::one_tap::service()).await?;
    let setup = protocol::MessagingSetup {
        id,
        qr_text: gateway::one_tap::qr(&waiting.link).context("the link is too long for a code")?,
        qr_svg: gateway::one_tap::qr_svg(&waiting.link)
            .context("the link is too long for a code")?,
        link: waiting.link.clone(),
        expires_in: waiting.remaining().as_secs(),
    };
    // Starting reaches the network, so an older begin can return after a newer one started.
    let mut pending = PENDING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if NEWEST.load(Ordering::SeqCst) != id {
        bail!("A newer setup replaced this one");
    }
    *pending = Some(Pending {
        id,
        stage: Stage::Waiting(Arc::new(waiting)),
    });
    // A window closed mid-setup must not leave the token in memory.
    tokio::spawn(async move {
        tokio::time::sleep(ABANDONED).await;
        cancel(id);
    });
    Ok(setup)
}

/// Waits a little for the setup's next step. Every wait below may be cut short without losing
/// the setup: its state changes only after a wait returns.
async fn advance(id: u64) -> Result<MessagingStep> {
    let started = Instant::now();
    let Ok(_turn) = tokio::time::timeout(STEP, STEPPING.lock()).await else {
        return Ok(step(&stage(id)?));
    };
    let left = || STEP.saturating_sub(started.elapsed());
    let outcome = match stage(id)? {
        Stage::Waiting(waiting) => {
            let within = left().saturating_sub(CONNECT);
            match tokio::time::timeout(within, waiting.wait_for(within)).await {
                Err(_) | Ok(Ok(None)) => Ok(None),
                Ok(Err(error)) => Err(error),
                // Kept before anything else is awaited, so the token cannot be dropped.
                Ok(Ok(Some(handed))) => {
                    let handed = Arc::new(handed);
                    replace(
                        id,
                        Stage::Created {
                            handed: Arc::clone(&handed),
                            telegram: None,
                            until: Instant::now() + FIRST_MESSAGE,
                        },
                    );
                    reach(id, handed, left()).await
                }
            }
        }
        Stage::Created {
            handed,
            telegram: None,
            ..
        } => reach(id, handed, left()).await,
        Stage::Created {
            handed,
            telegram: Some(telegram),
            until,
        } => match tokio::time::timeout(left(), first_message(&telegram, handed.owner)).await {
            Ok(Ok(owner)) => Ok(Some(Stage::Heard {
                handed,
                telegram,
                owner: Arc::new(owner),
            })),
            Ok(Err(error)) => Err(error),
            Err(_) if Instant::now() >= until => Err(anyhow!(
                "No message reached @{} within five minutes. Start setup again.",
                handed.bot
            )),
            Err(_) => Ok(None),
        },
        Stage::Heard { .. } => Ok(None),
    };
    match outcome {
        Ok(Some(next)) => replace(id, next),
        Ok(None) => {}
        Err(error) => {
            cancel(id);
            return Err(error);
        }
    }
    Ok(step(&stage(id)?))
}

/// Reaches the new bot; a slow network leaves it for the next step to try again.
async fn reach(id: u64, handed: Arc<Handed>, within: Duration) -> Result<Option<Stage>> {
    match tokio::time::timeout(within, Telegram::connect(&handed.token)).await {
        Err(_) => Ok(None),
        Ok(Err(error)) => Err(error),
        Ok(Ok(telegram)) => {
            let until = match stage(id)? {
                Stage::Created { until, .. } => until,
                _ => Instant::now() + FIRST_MESSAGE,
            };
            Ok(Some(Stage::Created {
                handed,
                telegram: Some(Arc::new(telegram)),
                until,
            }))
        }
    }
}

fn step(stage: &Stage) -> MessagingStep {
    match stage {
        Stage::Waiting(waiting) => MessagingStep::Waiting {
            expires_in: waiting.remaining().as_secs(),
        },
        // Reaching the bot clears what was sent before, so a message is asked for only after.
        Stage::Created {
            telegram: None,
            until,
            ..
        } => MessagingStep::Waiting {
            expires_in: until.saturating_duration_since(Instant::now()).as_secs(),
        },
        Stage::Created { handed, .. } => MessagingStep::Created {
            bot: handed.bot.clone(),
        },
        Stage::Heard { handed, owner, .. } => MessagingStep::Heard {
            bot: handed.bot.clone(),
            name: owner.name.clone(),
            user: owner.user.clone(),
        },
    }
}

fn stage(id: u64) -> Result<Stage> {
    let pending = PENDING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match pending.as_ref() {
        Some(held) if held.id == id => Ok(held.stage.clone()),
        Some(_) => bail!("A newer setup replaced this one"),
        None => bail!("This setup ended. Start setup again."),
    }
}

/// Moves the named setup on, unless it was cancelled or replaced while it waited.
fn replace(id: u64, next: Stage) {
    let mut pending = PENDING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(held) = pending.as_mut().filter(|held| held.id == id) {
        held.stage = next;
    }
}

/// Drops the named setup, or every one.
fn cancel(id: u64) {
    let mut pending = PENDING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if pending
        .as_ref()
        .is_some_and(|held| id == u64::MAX || held.id == id)
    {
        pending.take();
    }
}

/// The person confirmed the name shown: the setup is taken out, so it is saved once.
async fn save_pending(
    id: u64,
    folder: Option<PathBuf>,
    workspace: &Path,
) -> Result<MessagingSaved> {
    let folder = chosen_folder(folder, workspace)?;
    let stage = {
        let mut pending = PENDING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match pending.take() {
            Some(held) if held.id == id && matches!(held.stage, Stage::Heard { .. }) => held.stage,
            other => {
                *pending = other;
                bail!("Wait until your first message to the bot is shown here");
            }
        }
    };
    let Stage::Heard {
        handed,
        telegram,
        owner,
    } = stage
    else {
        unreachable!("only a heard setup is taken out");
    };
    save(
        "telegram",
        &handed.token,
        &format!("@{}", handed.bot),
        &folder,
        Some((&owner, &telegram)),
    )
    .await
}

/// The folder asked for, else the one saved before, else the open workspace; never one far
/// broader than a project.
fn chosen_folder(asked: Option<PathBuf>, workspace: &Path) -> Result<PathBuf> {
    let saved = config::load()?.unwrap_or_default().gateway.folder;
    let folder = asked.or(saved).unwrap_or_else(|| workspace.to_path_buf());
    runtime::messaging::admit_folder(&folder)
}

/// Waits for the creator's first private message to the new bot; others writing are ignored.
pub(crate) async fn first_message(telegram: &Telegram, creator: i64) -> Result<Owner> {
    let creator = creator.to_string();
    loop {
        for heard in telegram.hear().await? {
            if let Heard::Message(message) = heard
                && !message.group
                && message.user == creator
            {
                return Ok(Owner {
                    user: message.user,
                    name: message.name,
                    chat: message.chat,
                });
            }
        }
    }
}

/// Saves the token and the folder, pairs the owner when there is one, and starts the gateway.
pub(crate) async fn save(
    platform: &str,
    token: &str,
    account: &str,
    folder: &Path,
    owner: Option<(&Owner, &Telegram)>,
) -> Result<MessagingSaved> {
    config::store_key(&credential_id(platform), token)?;
    let mut cfg = config::load()?.unwrap_or_default();
    cfg.gateway.make_default(folder.to_path_buf());
    let saved = cfg
        .gateway
        .platforms
        .entry(platform.to_owned())
        .or_insert_with(|| Platform {
            enabled: true,
            settings: Default::default(),
        });
    saved.enabled = true;
    saved.settings.insert(ACCOUNT.into(), account.to_owned());
    // Only a scanned bot comes with its owner confirmed; a pasted token's bot takes codes.
    match owner {
        Some(_) => saved.settings.insert(MADE.into(), SCANNED.into()),
        None => saved.settings.remove(MADE),
    };
    config::save(&cfg)?;
    if let Some((owner, telegram)) = owner {
        Pairing::open()?.admit(platform, &owner.user, &owner.name)?;
        runtime::messaging::set_home(platform, &owner.chat)?;
        // Before the gateway starts, which takes over the bot's messages; a slow send is let go.
        let _ = tokio::time::timeout(CONNECT, telegram.send(&owner.chat, PAIRED)).await;
    }
    let problem = crate::gateway_service::install()
        .err()
        .map(|error| format!("{error:#}"));
    Ok(MessagingSaved {
        account: account.to_owned(),
        folder: folder.to_path_buf(),
        paired: owner.is_some(),
        problem,
    })
}
